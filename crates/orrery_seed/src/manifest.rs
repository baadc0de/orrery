//! The content manifest (docs/12-world-seeding.md §9.3).
//!
//! One entry per seeded row:
//!
//! ```text
//! (ContentKey, PersistId, grid, cell, value_digest, byte_len, archetype, layer, emit)
//! ```
//!
//! streamed in **`(grid, cell, ContentKey)` ascending** order — which is
//! generation order, so the manifest streams out with no sort pass. The
//! rolling digest covers the entries; a toolchain stamp records the build so
//! a golden-manifest CI test shifts as a reviewed diff on a toolchain bump
//! (§8, §15).
//!
//! **`value_digest` covers the component bag only** — never the key, and
//! never the storage value's one-byte live/tombstone tag (P-6, and P2
//! decision C-4 in docs/11-roadmap.md §P2). The tag is storage framing, not
//! content: the seeder computes the digest from `SeedEncoder`'s output
//! before it knows anything about how the row will be framed, which is what
//! makes gate A4 ("identical manifest digest, zero rows changed") mean the
//! same thing to the seeder, to `verify --full`, and to a cell actor
//! re-checkpointing an untouched row.

use orrery_protocol::{CellId, GridId, PersistId};

use crate::content::ContentKey;

/// One manifest row (docs/12 §9.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    /// The derivation-path identity (§9.1).
    pub content_key: ContentKey,
    /// The minted id (block-granted from `pid/next` by the writer; a
    /// deterministic per-cell counter under `plan`).
    pub persist_id: PersistId,
    /// The entity's grid.
    pub grid: GridId,
    /// The entity's own interest cell (P-2).
    pub cell: CellId,
    /// blake3 over the **component bag only** (C-4): 16 bytes.
    pub value_digest: [u8; 16],
    /// The bag length in bytes (no tag, no key).
    pub byte_len: u32,
    /// The archetype name.
    pub archetype: String,
    /// The layer that produced the field this row was realized from.
    pub layer: String,
    /// The emit name.
    pub emit: String,
}

/// The manifest's toolchain stamp (docs/12 §8: "the manifest records the
/// toolchain"), so a golden-manifest shift on a toolchain bump is a reviewed
/// diff rather than a mystery failure (§15).
///
/// The rustc version is read once at process start (the stamp is a manifest
/// input, not a compile-time constant — the same binary must report the
/// toolchain it was built with, which the workspace pins via
/// `rust-toolchain.toml`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolchainStamp {
    /// `rustc --version` of the building toolchain.
    pub rustc: String,
    /// The target triple (compile-time: it cannot change for one binary).
    pub target: &'static str,
    /// The crate version.
    pub version: &'static str,
}

impl ToolchainStamp {
    /// The current build's stamp. Deterministic within a pinned toolchain —
    /// which is exactly when golden manifests are valid (§15): the workspace
    /// pins the channel in `rust-toolchain.toml`, so the stamp reads the
    /// pinned version from the environment at *build* time when available
    /// and otherwise uses the workspace-pinned channel string. It never
    /// probes the filesystem at runtime.
    #[must_use]
    pub fn current() -> Self {
        Self {
            rustc: option_env!("RUSTC_VERSION")
                .unwrap_or(rustc_channel())
                .to_string(),
            target: std::env::consts::ARCH,
            version: env!("CARGO_PKG_VERSION"),
        }
    }
}

/// The workspace-pinned toolchain channel (rust-toolchain.toml), the stamp
/// fallback. Keep in sync with `rust-toolchain.toml` — the pin, not this
/// string, is normative.
fn rustc_channel() -> &'static str {
    // option_env! cannot read rust-toolchain.toml; the channel is a workspace
    // constant. If the pin changes, this string (and the golden manifests)
    // change with it as one reviewed diff.
    "rustc 1.96.0"
}

/// The rolling manifest digest: a blake3 hasher fed each entry's canonical
/// bytes in stream order, plus the toolchain stamp at the end.
///
/// Canonical entry encoding (fixed widths, no separators — the same
/// discipline as [`ContentKey::derive`]):
///
/// ```text
/// content_key(16) ‖ persist_id(8, BE) ‖ grid(4, BE) ‖ cell(8, BE)
/// ‖ value_digest(16) ‖ byte_len(4, LE) ‖ archetype ‖ 0x00 ‖ layer ‖ 0x00
/// ‖ emit ‖ 0x00
/// ```
///
/// The name fields are NUL-terminated because they are variable-length;
/// NUL is not a valid TOML bare-key character, so termination is
/// unambiguous.
#[derive(Debug, Clone)]
pub struct ManifestDigest {
    hasher: blake3::Hasher,
    entries: u64,
}

impl Default for ManifestDigest {
    fn default() -> Self {
        Self::new()
    }
}

impl ManifestDigest {
    /// A fresh digest state, domain-tagged.
    #[must_use]
    pub fn new() -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"orrery.manifest.v1");
        Self { hasher, entries: 0 }
    }

    /// Fold one entry into the digest, in stream order.
    pub fn push(&mut self, e: &ManifestEntry) {
        self.hasher.update(&e.content_key.0);
        self.hasher.update(&e.persist_id.0.to_be_bytes());
        self.hasher.update(&e.grid.0.to_be_bytes());
        self.hasher.update(&e.cell.to_bits().to_be_bytes());
        self.hasher.update(&e.value_digest);
        self.hasher.update(&e.byte_len.to_le_bytes());
        self.hasher.update(e.archetype.as_bytes());
        self.hasher.update(&[0]);
        self.hasher.update(e.layer.as_bytes());
        self.hasher.update(&[0]);
        self.hasher.update(e.emit.as_bytes());
        self.hasher.update(&[0]);
        self.entries += 1;
    }

    /// Number of entries folded so far.
    #[must_use]
    pub fn entries(&self) -> u64 {
        self.entries
    }

    /// Finalize with the toolchain stamp and the entry count, returning the
    /// 32-byte manifest digest.
    #[must_use]
    pub fn finalize(self, stamp: &ToolchainStamp) -> [u8; 32] {
        let mut hasher = self.hasher;
        hasher.update(&self.entries.to_le_bytes());
        hasher.update(stamp.rustc.as_bytes());
        hasher.update(&[0]);
        hasher.update(stamp.target.as_bytes());
        hasher.update(&[0]);
        hasher.update(stamp.version.as_bytes());
        *hasher.finalize().as_bytes()
    }
}

/// A streaming manifest writer: accepts entries in generation order and
/// maintains the rolling digest. Enforces the canonical order
/// (`(grid, cell, ContentKey)` ascending) so an out-of-order producer fails
/// loudly rather than writing an unsplittable manifest.
#[derive(Debug)]
pub struct ManifestWriter {
    digest: ManifestDigest,
    last: Option<(GridId, CellId, ContentKey)>,
}

impl Default for ManifestWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl ManifestWriter {
    /// A fresh writer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            digest: ManifestDigest::new(),
            last: None,
        }
    }

    /// Push one entry. Must be in `(grid, cell, ContentKey)` ascending order
    /// (§9.3: generation order).
    ///
    /// # Panics
    ///
    /// Panics on an out-of-order entry: the manifest is defined as streamed
    /// in canonical order with no sort pass, and a producer that cannot
    /// deliver that order is a bug, not a data condition.
    pub fn push(&mut self, e: ManifestEntry) {
        let key = (e.grid, e.cell, e.content_key);
        if let Some(last) = self.last {
            assert!(
                key > last,
                "manifest entries must stream in (grid, cell, ContentKey) ascending order (§9.3)"
            );
        }
        self.last = Some(key);
        self.digest.push(&e);
    }

    /// Entries so far.
    #[must_use]
    pub fn entries(&self) -> u64 {
        self.digest.entries()
    }

    /// Finalize into the digest.
    #[must_use]
    pub fn finish(self, stamp: &ToolchainStamp) -> [u8; 32] {
        self.digest.finalize(stamp)
    }
}

/// blake3[..16] over the component bag (C-4: the bag only — no key, no tag).
#[must_use]
pub fn value_digest(bag: &[u8]) -> [u8; 16] {
    let digest = blake3::hash(bag);
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest.as_bytes()[..16]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(cell_bits: u64, index: u64, emit: &str) -> ManifestEntry {
        ManifestEntry {
            content_key: ContentKey([index as u8; 16]),
            persist_id: PersistId::new(index),
            grid: GridId::ROOT,
            cell: CellId::from_bits(cell_bits).expect("nonzero"),
            value_digest: [0xEE; 16],
            byte_len: 256,
            archetype: "crate".to_string(),
            layer: "world".to_string(),
            emit: emit.to_string(),
        }
    }

    #[test]
    fn manifest_streams_in_canonical_order() {
        // §9.3: (grid, cell, ContentKey) ascending = generation order. The
        // writer enforces it: ascending is accepted…
        let mut w = ManifestWriter::new();
        w.push(entry(0xA924_9249_2492_4D65, 1, "props"));
        w.push(entry(0xA924_9249_2492_4D66, 2, "props"));
        w.push(entry(0xA924_9249_2492_4D67, 3, "props"));
        assert_eq!(w.entries(), 3);
    }

    #[test]
    #[should_panic(expected = "ascending order")]
    fn manifest_rejects_out_of_order_entries() {
        // …and a producer that breaks the order fails loudly rather than
        // writing a manifest that cannot be diffed by ContentKey.
        let mut w = ManifestWriter::new();
        w.push(entry(0xA924_9249_2492_4D66, 2, "props"));
        w.push(entry(0xA924_9249_2492_4D65, 1, "props"));
    }

    #[test]
    fn manifest_digest_is_order_sensitive() {
        // Same entries, different order → different digest. (The writer
        // forbids the reordering; this drives the digest directly to prove
        // the digest actually covers order, i.e. it is a rolling hash, not a
        // set xor.)
        let e1 = entry(0xA924_9249_2492_4D65, 1, "props");
        let e2 = entry(0xA924_9249_2492_4D66, 2, "props");
        let stamp = ToolchainStamp::current();

        let mut a = ManifestDigest::new();
        a.push(&e1);
        a.push(&e2);
        let da = a.finalize(&stamp);

        let mut b = ManifestDigest::new();
        b.push(&e2);
        b.push(&e1);
        let db = b.finalize(&stamp);
        assert_ne!(da, db, "the rolling digest covers entry order");
    }

    #[test]
    fn value_digest_covers_bag_only() {
        // C-4: the digest covers the bag, not the key, not the tag. Two bags
        // differing only in content differ; the same bag digests identically
        // regardless of what key or tag a writer would attach.
        let bag = b"hp=100";
        assert_eq!(value_digest(bag), value_digest(bag));
        assert_ne!(value_digest(bag), value_digest(b"hp=101"));
        // And it differs from a digest of the tagged row (a writer framing
        // the bag must not change the manifest).
        let tagged = orrery_persistd::keyspace::encode_live_value(bag);
        assert_ne!(
            value_digest(bag),
            value_digest(&tagged),
            "the tag is storage framing; the manifest digest excludes it"
        );
    }
}
