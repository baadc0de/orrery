//! The component-bag seam (docs/12-world-seeding.md §4.1).
//!
//! `EntityRecord.components` is opaque postcard bytes; the cell actor never
//! decodes game types and neither does the seeder. The seam is one trait —
//! [`SeedEncoder`] — implemented in the game's crate and linked into the
//! game's seeder binary. The shipped binary links [`OpaqueEncoder`], which
//! fills bags of the declared size with deterministic filler: enough for
//! everything the P2 demo measures (row counts, byte volumes, range-scan
//! behaviour, checkpoint sizes, restart recovery), because none of those
//! care what is *in* the bag.
//!
//! **The encoder returns the bag only.** The `LIVE_TAG`/`TOMBSTONE_TAG`
//! prefix is storage framing (P-6, C-4) and belongs to the writer —
//! `orrery_persistd::keyspace::encode_live_value` — never to the encoder.
//! The manifest's `value_digest` covers the bag alone for the same reason.

use bytes::Bytes;
use orrery_protocol::{CellId, GridId, PersistId};
use rand_chacha::ChaCha8Rng;

use crate::content::ContentKey;
use crate::scenario::ArchetypeFields;

/// The hex escape hatch ceiling (docs/12 §4.1): `bytes = "0x…"` exists for
/// fixtures — the one hand-authored row a regression test needs — and is
/// capped at 4 KiB. It is not a substitute for an encoder.
pub const HEX_ESCAPE_CAP: usize = 4 * 1024;

/// Everything an encoder needs to fill one entity's bag (docs/12 §4.1).
pub struct EncodeCtx<'a> {
    /// The archetype name from `[archetype.<name>]`.
    pub archetype: &'a str,
    /// The archetype's resolved `fields` table, passed through uninterpreted
    /// (§5.5: the seeder does not interpret it).
    pub fields: &'a ArchetypeFields,
    /// The entity's own interest cell (P-2).
    pub cell: CellId,
    /// Its grid.
    pub grid: GridId,
    /// Metres within the cell, from [`crate::place::hash_local_pos`].
    pub local_pos: [f32; 3],
    /// The derivation-path identity (docs/12 §9.1).
    pub content_key: ContentKey,
    /// The minted id (allocated per cell in plan mode; block-granted from
    /// `pid/next` by the writer, §9.2).
    pub persist_id: PersistId,
    /// The slot RNG, seeded from `K_slot` (§8) — an encoder that needs
    /// randomness draws here and nowhere else.
    pub rng: &'a mut ChaCha8Rng,
}

/// An encoder failure. Carries the archetype name when the failure is
/// archetype-specific so the plan can name the offending table.
#[derive(Debug)]
pub struct EncodeError(pub String);

impl core::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "encode error: {}", self.0)
    }
}

impl core::error::Error for EncodeError {}

/// A game's bridge from scenario archetypes to component bags (docs/12
/// §4.1). Implemented in the game's crate; linked into the game's seeder
/// binary.
/// D51 intentionally keeps this trait limited to entity component bags.
pub trait SeedEncoder: Send + Sync {
    /// Encode one entity's component bag from its archetype and derived
    /// context. Returns the **bag only** — the writer prepends the storage
    /// tag (`LIVE_TAG`), never the encoder (C-4).
    fn encode(&self, ctx: &EncodeCtx<'_>) -> Result<Bytes, EncodeError>;

    /// Declared bag size for an archetype, for byte-budget estimation without
    /// encoding (docs/12 §4.1). Must be an upper bound; `plan` reports
    /// measured vs declared. `None` when the encoder cannot say.
    fn declared_size(&self, archetype: &str) -> Option<usize>;

    // D51 keeps the extension point entity-only until a future owner decision.
}

/// The built-in encoder for `[payload] class = "opaque"` (docs/12 §4.1):
/// emits a postcard-encoded `(schema_version: u16, size: u32, filler)` bag of
/// the archetype's declared size. The filler is a deterministic byte pattern
/// derived from the slot RNG — the bag must be compressible to nothing
/// interesting but must not be all zeros, so a checkpoint-size estimate that
/// accidentally measured a sparse file would still be honest.
#[derive(Debug, Default, Clone, Copy)]
pub struct OpaqueEncoder;

/// The postcard body of an opaque bag: a header plus filler (docs/12 §4.1).
/// The `size` field records the declared size so a reader can tell a
/// truncated bag from a small one.
#[derive(Debug, serde::Serialize)]
struct OpaqueBag<'a> {
    schema_version: u16,
    size: u32,
    filler: &'a [u8],
}

/// The byte width of postcard's varint encoding of `n` (LEB128, the scheme
/// postcard uses for `usize` sequence lengths).
fn varint_len(mut n: usize) -> usize {
    let mut len = 1;
    while n >= 0x80 {
        n >>= 7;
        len += 1;
    }
    len
}

impl OpaqueEncoder {
    /// The postcard framing overhead of an [`OpaqueBag`] (header + the
    /// filler's varint length prefix).
    ///
    /// postcard varint-encodes the sequence length, so the overhead depends
    /// on the filler length itself — a fixpoint: `overhead(fl)` is the
    /// serialized size of everything except the filler bytes. We solve it by
    /// measuring the header with an empty filler and then iterating
    /// `fl = declared − header − varint_len(fl)` to convergence (at most a
    /// handful of steps; the varint width changes only at powers of 128).
    fn framing_overhead(schema_version: u16, declared: usize) -> Result<usize, EncodeError> {
        // Serialized size of the bag with an EMPTY filler: header fields
        // plus a 1-byte zero-length varint.
        let empty = postcard::experimental::serialized_size(&OpaqueBag {
            schema_version,
            size: declared as u32,
            filler: &[],
        })
        .map_err(|e| EncodeError(format!("opaque bag framing: {e}")))?;
        // `empty - 1` is the header without the length prefix. Now find the
        // filler length `fl` with `fl + (empty - 1) + varint_len(fl) ==
        // declared`.
        let header = empty - 1;
        if declared < header + 1 {
            // Below the smallest possible framing (header + one varint byte):
            // an error naming the framing, not a silent clamp (the caller's
            // declared_size is wrong).
            return Err(EncodeError(format!(
                "declared size {declared}B is below the postcard framing overhead of {}B",
                header + 1
            )));
        }
        // Iterate the fixpoint; start from a 1-byte-varint guess.
        let mut fl = declared.saturating_sub(header + 1);
        for _ in 0..8 {
            let width = varint_len(fl);
            let next = declared.saturating_sub(header + width);
            if next == fl {
                break;
            }
            fl = next;
        }
        Ok(declared - fl)
    }

    /// Fill `buf` from the slot RNG (docs/12 §8: every draw is addressed by
    /// the slot; the bag's bytes are a pure function of `K_slot`).
    ///
    /// `SeedEncoder::encode` takes `&EncodeCtx`, so the `&mut ChaCha8Rng`
    /// field cannot be advanced in place — the filler stream is drawn from a
    /// **clone** of the slot RNG instead (ChaCha8Rng is a pure function of
    /// its state, so cloning is the deterministic reborrow the `&` receiver
    /// requires). The filler stays a pure function of `K_slot`; the caller's
    /// RNG position is undisturbed.
    fn fill(ctx: &EncodeCtx<'_>, buf: &mut [u8]) {
        use rand::RngCore;
        let mut rng = ctx.rng.clone();
        rng.fill_bytes(buf);
    }
}

impl SeedEncoder for OpaqueEncoder {
    fn encode(&self, ctx: &EncodeCtx<'_>) -> Result<Bytes, EncodeError> {
        let declared = ctx.fields.declared_size_bytes.ok_or_else(|| {
            EncodeError(format!(
                "opaque archetype {:?} declares no size",
                ctx.archetype
            ))
        })?;
        let schema_version = ctx.fields.schema_version;
        let overhead = Self::framing_overhead(schema_version, declared)?;
        if declared < overhead {
            return Err(EncodeError(format!(
                "declared size {declared}B for {:?} is below the postcard framing overhead {overhead}B",
                ctx.archetype
            )));
        }
        let filler_len = declared - overhead;
        let mut filler = vec![0u8; filler_len];
        Self::fill(ctx, &mut filler);
        let bag = OpaqueBag {
            schema_version,
            size: declared as u32,
            filler: &filler,
        };
        postcard::to_stdvec(&bag)
            .map(Bytes::from)
            .map_err(|e| EncodeError(format!("opaque bag encode: {e}")))
    }

    fn declared_size(&self, archetype: &str) -> Option<usize> {
        // The opaque encoder cannot know an archetype's declared size without
        // its fields table; the caller that has the table reads
        // `declared_size_bytes` directly. Returning `None` keeps the trait
        // honest (§4.1: "None when the encoder cannot say").
        let _ = archetype;
        None
    }
}

/// Encode the hex escape hatch (docs/12 §4.1): `bytes = "0x…"` in an
/// archetype, capped at [`HEX_ESCAPE_CAP`]. Returns the bag bytes exactly as
/// written — this is how a regression test authors one fixed row.
pub fn encode_hex_escape(hex: &str) -> Result<Bytes, EncodeError> {
    let hex = hex.strip_prefix("0x").unwrap_or(hex);
    if !hex.len().is_multiple_of(2) {
        return Err(EncodeError(format!(
            "hex escape has an odd digit count ({})",
            hex.len()
        )));
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let mut i = 0;
    while i < hex.len() {
        let byte = u8::from_str_radix(&hex[i..i + 2], 16)
            .map_err(|e| EncodeError(format!("hex escape: {e}")))?;
        out.push(byte);
        i += 2;
    }
    if out.len() > HEX_ESCAPE_CAP {
        return Err(EncodeError(format!(
            "hex escape of {}B exceeds the {}B cap (docs/12 §4.1)",
            out.len(),
            HEX_ESCAPE_CAP
        )));
    }
    Ok(Bytes::from(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seedtree::SeedRoot;
    use orrery_protocol::CellId;

    fn ctx<'a>(fields: &'a ArchetypeFields, rng: &'a mut ChaCha8Rng) -> EncodeCtx<'a> {
        EncodeCtx {
            archetype: "prop",
            fields,
            cell: CellId::from_bits(0xA924_9249_2492_4D65).expect("nonzero"),
            grid: GridId::ROOT,
            local_pos: [1.0, 2.0, 3.0],
            content_key: ContentKey([0xAB; 16]),
            persist_id: PersistId::new(42),
            rng,
        }
    }

    #[test]
    fn encoder_output_carries_no_value_tag() {
        // C-4 / docs/12 §4.1: the encoder returns the bag ONLY. The
        // LIVE_TAG/TOMBSTONE_TAG prefix belongs to the writer
        // (orrery_persistd::keyspace::encode_live_value). Assert the encoded
        // bag does not begin with a storage tag that a writer would then
        // double-prefix: the bag's first byte is the postcard varint of
        // schema_version (1), not 0x00.
        let mut rng = SeedRoot::slot_rng([5u8; 32]);
        let fields = ArchetypeFields {
            declared_size_bytes: Some(256),
            schema_version: 1,
            bytes_hex: None,
            table: toml::Table::new(),
        };
        let bag = OpaqueEncoder
            .encode(&ctx(&fields, &mut rng))
            .expect("encodes");
        assert_eq!(bag.len(), 256, "the bag is exactly the declared size");
        assert_ne!(
            bag[0],
            orrery_persistd::keyspace::LIVE_TAG,
            "the bag's first byte is content (schema_version varint), never the writer's tag"
        );
        // The writer's framing is additive: LIVE_TAG ‖ bag.
        let framed = orrery_persistd::keyspace::encode_live_value(&bag);
        assert_eq!(framed[0], orrery_persistd::keyspace::LIVE_TAG);
        assert_eq!(&framed[1..], &bag[..]);
    }

    #[test]
    fn opaque_bag_is_deterministic_per_slot() {
        // The bag is a pure function of K_slot (§8): same slot key, same
        // bytes, no matter what else ran before.
        let fields = ArchetypeFields {
            declared_size_bytes: Some(192),
            schema_version: 3,
            bytes_hex: None,
            table: toml::Table::new(),
        };
        let mut rng_a = SeedRoot::slot_rng([6u8; 32]);
        let mut rng_b = SeedRoot::slot_rng([6u8; 32]);
        let a = OpaqueEncoder.encode(&ctx(&fields, &mut rng_a)).expect("a");
        let b = OpaqueEncoder.encode(&ctx(&fields, &mut rng_b)).expect("b");
        assert_eq!(a, b);
        assert_eq!(a.len(), 192);
        // And the filler is not all zeros (the honest-compression property).
        assert!(a.iter().any(|&b| b != 0));
    }

    #[test]
    fn opaque_declared_size_below_framing_is_an_error() {
        let mut rng = SeedRoot::slot_rng([7u8; 32]);
        let fields = ArchetypeFields {
            declared_size_bytes: Some(2),
            schema_version: 1,
            bytes_hex: None,
            table: toml::Table::new(),
        };
        let err = OpaqueEncoder.encode(&ctx(&fields, &mut rng)).unwrap_err();
        assert!(
            err.to_string().contains("framing overhead"),
            "names the framing: {err}"
        );
    }

    #[test]
    fn hex_escape_hatch_decodes_and_caps() {
        let bytes = encode_hex_escape("0xdeadbeef").expect("decodes");
        assert_eq!(&bytes[..], &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert!(encode_hex_escape("0xabc").is_err(), "odd digit count");
        assert!(encode_hex_escape("0xzz").is_err(), "bad hex");
        let too_big = format!("0x{}", "00".repeat(HEX_ESCAPE_CAP + 1));
        let err = encode_hex_escape(&too_big).unwrap_err();
        assert!(err.to_string().contains("cap"), "names the cap: {err}");
        let at_cap = format!("0x{}", "00".repeat(HEX_ESCAPE_CAP));
        assert!(
            encode_hex_escape(&at_cap).is_ok(),
            "exactly at the cap is fine"
        );
    }
}
