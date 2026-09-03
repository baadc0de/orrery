//! Read-back verification for seeded worlds.

use std::path::PathBuf;

#[cfg(feature = "fdb")]
use orrery_persistd::keyspace;

#[cfg(feature = "fdb")]
use crate::apply;
use crate::manifest::{ManifestSink, ToolchainStamp};
use crate::scenario::ResolvedScenario;

/// Verification options.
#[derive(Debug, Clone)]
pub struct VerifyOptions {
    /// Check every row rather than sampling.
    pub full: bool,
    /// Emit the manifest to this path.
    pub emit_manifest: Option<PathBuf>,
    /// Flatten nested grids into grid 0.
    pub single_grid: bool,
}

/// Verification summary.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    /// Rows checked.
    pub checked_rows: u64,
    /// Output manifest path, if any.
    pub emit_manifest: Option<PathBuf>,
}

/// Rebuild the seeded world from the scenario and compare it to the live
/// FDB rows, optionally emitting the manifest snapshot.
#[cfg(feature = "fdb")]
pub async fn run(
    source: &str,
    mut scenario: ResolvedScenario,
    options: VerifyOptions,
) -> Result<VerifyReport, String> {
    if options.single_grid {
        // Use the same flattening as apply to keep the read side aligned.
        scenario = flatten(scenario);
    }
    let seed_display = String::from_utf8_lossy(&scenario.seed_material).to_string();
    let root = crate::seedtree::SeedRoot::derive(&scenario.seed_context, &scenario.seed_material);
    let content_build = scenario
        .raw
        .scenario
        .content_build
        .clone()
        .unwrap_or_else(|| scenario.raw.scenario.name.clone());

    let db = crate::fdb_open(&crate::cluster_file_from_env()?)?;
    let existing_seedmap = crate::idmap::read_seedmap(&db).await?;
    let config_digest = blake3::hash(source.as_bytes()).to_hex().to_string();
    let desired = apply::build_desired_rows(
        &db,
        &scenario,
        &root,
        &existing_seedmap,
        apply::ContentStamp {
            content_build: &content_build,
            seed_display: &seed_display,
            config_digest: &config_digest,
            // `verify` does not seal: the fingerprint is whatever the cluster
            // already holds, read back below. Recomputing a row that claims a
            // seal the cluster never received is the one thing a read-back
            // check must not do.
            universe_seed_fingerprint: None,
        },
    )
    .await?;
    let existing = apply::load_existing_rows(&db, &scenario, &desired).await?;
    let checked_rows = desired
        .iter()
        .filter(|row| existing.get(&row.key) == Some(&row.value))
        .count() as u64;

    if let Some(path) = &options.emit_manifest {
        // The trailer is the `content/version` row this same pass built, so
        // the manifest carries the digest the cluster records rather than a
        // separately derived one that could disagree with it.
        let mut record = desired
            .iter()
            .find(|row| row.key == keyspace::content_version_key().to_vec())
            .map(|row| orrery_persistd::content_version::decode(&row.value))
            .transpose()?;
        // …with one field that is *not* recomputable: the universe seed
        // fingerprint was supplied at `apply` time and exists only in the
        // cluster. Carrying it into the artifact is what lets `verify` later
        // cross-check a cluster against a manifest offline, which is the whole
        // reason the seal lives in this record rather than in a row of its own.
        if let Some(record) = record.as_mut() {
            record.universe_seed_fingerprint = existing
                .get(keyspace::content_version_key().as_slice())
                .map(|bytes| orrery_persistd::content_version::decode(bytes))
                .transpose()?
                .and_then(|durable| durable.universe_seed_fingerprint);
        }
        write_manifest(
            path,
            desired.iter().filter_map(|row| row.manifest.as_ref()),
            record.as_ref(),
        )?;
    }

    let _ = source;
    let _ = options.full;
    Ok(VerifyReport {
        checked_rows,
        emit_manifest: options.emit_manifest,
    })
}

/// Write a §9.3 manifest to `path`: line-delimited JSON, one entry per line,
/// streamed straight through so nothing accumulates.
///
/// `entries` is an iterator rather than a slice on purpose. The old shape
/// collected every entry into a `Vec` and handed the lot to
/// `serde_json::to_vec_pretty`, which meant two more full copies of a
/// document §9.3 sizes at 470 MB for 10 M entities — and produced one
/// pretty-printed array, which a consumer cannot read until it has read all
/// of it. Taking an iterator means the caller cannot accidentally reintroduce
/// that: peak memory here is one entry's line.
///
/// `record` is the `content/version` tuple, written as the last line. It
/// cannot be a header: its `manifest_digest` covers every entry, so it does
/// not exist until the stream is done.
///
/// Returns the manifest digest, which is computed over the entries' canonical
/// fixed-width encoding — never over these JSON bytes, so the serialization
/// change leaves gate A4 and `content/version` untouched.
///
/// One ceiling this does *not* lift: `verify` still builds the whole desired
/// row set (`apply::build_desired_rows`) and the whole existing set before it
/// gets here, so the *seeder* is still O(world) in memory even though the
/// manifest is not. Fixing that means making the desired-row pass an iterator
/// too, which is a change to the apply path, not to the manifest format.
///
/// # Errors
///
/// Returns a message on a create, write or flush failure.
pub fn write_manifest<'a, I, R>(
    path: &std::path::Path,
    entries: I,
    record: Option<&R>,
) -> Result<[u8; 32], String>
where
    I: IntoIterator<Item = &'a crate::manifest::ManifestEntry>,
    R: serde::Serialize,
{
    let file = std::fs::File::create(path).map_err(|e| format!("write manifest: {e}"))?;
    let mut sink = ManifestSink::new(std::io::BufWriter::new(file));
    for entry in entries {
        sink.push(entry)
            .map_err(|e| format!("write manifest: {e}"))?;
    }
    let stamp = ToolchainStamp::current();
    match record {
        Some(record) => sink.finish(record, &stamp),
        None => sink.finish_without_record(&stamp),
    }
    .map_err(|e| format!("write manifest: {e}"))
}

/// Stand-in for the verify path when the `fdb` feature is off: without a
/// cluster there is nothing to verify against, so this reports rather than
/// pretending to have checked.
#[cfg(not(feature = "fdb"))]
pub async fn run(
    _source: &str,
    _scenario: ResolvedScenario,
    options: VerifyOptions,
) -> Result<VerifyReport, String> {
    let _ = options;
    Err("verify requires the `fdb` feature".to_string())
}

#[cfg(feature = "fdb")]
fn flatten(mut scenario: ResolvedScenario) -> ResolvedScenario {
    let root = scenario
        .grids
        .get(&0)
        .copied()
        .unwrap_or(crate::scenario::ResolvedGrid {
            id: orrery_protocol::GridId::ROOT,
            cell_edge_m: orrery_protocol::DEFAULT_CELL_EDGE_M,
        });
    scenario.grids.clear();
    scenario.grids.insert(0, root);
    for layer in &mut scenario.layers {
        layer.grid = orrery_protocol::GridId::ROOT;
        layer.cell_edge_m = root.cell_edge_m;
    }
    for emit in &mut scenario.emits {
        emit.grid = orrery_protocol::GridId::ROOT;
    }
    scenario
}

#[cfg(test)]
mod tests {
    use orrery_protocol::{CellId, GridId, PersistId};

    use super::write_manifest;
    use crate::apply::ContentVersion;
    use crate::content::ContentKey;
    use crate::manifest::ManifestEntry;

    fn entry(i: u64) -> ManifestEntry {
        let mut key = [0u8; 16];
        key[..8].copy_from_slice(&i.to_be_bytes());
        ManifestEntry {
            content_key: ContentKey(key),
            persist_id: PersistId::new(i + 1),
            grid: GridId::ROOT,
            cell: CellId::from_bits(0xA924_9249_2492_4D65).expect("nonzero"),
            value_digest: [0xEE; 16],
            byte_len: 256,
            archetype: "crate".to_string(),
            layer: "world".to_string(),
            emit: "props".to_string(),
        }
    }

    /// The P2 load rig's manifest decoder, transcribed from
    /// `gates/p2-load/src/main.rs`. That file belongs to another change in flight,
    /// so this is a *copy* of its shape rather than a call into it: if the
    /// rig's decoder is edited, this fixture has to be re-checked against it.
    /// It exists because the rig is the manifest's only in-tree consumer and
    /// `scripts/p2-kill9-gate.sh` feeds it the file this module writes.
    mod p2_rig {
        use orrery_protocol::{CellId, GridId, PersistId};

        #[derive(Debug, serde::Deserialize)]
        #[serde(untagged)]
        pub enum ManifestLine {
            Header {
                #[allow(dead_code)]
                content_version: serde_json::Value,
            },
            Entry(Entry),
        }

        #[derive(Debug, serde::Deserialize)]
        pub struct Entry {
            pub persist_id: PersistId,
            pub cell: CellId,
            pub grid: Option<GridId>,
        }
    }

    #[test]
    fn the_p2_rig_decoder_reads_what_the_seeder_writes() {
        // The gate's chain is: `verify --emit-manifest` → `gates/p2-load
        // --manifest` → a lease claim per entity at the cell the seeder
        // committed it to. A format change that the rig cannot read breaks
        // the P2 kill-9 gate, so the decode is exercised here rather than
        // assumed.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("manifest.json");
        let entries: Vec<_> = (0..8).map(entry).collect();
        let record = ContentVersion {
            content_build: "b".to_string(),
            manifest_digest: "deadbeef".to_string(),
            scenario_seed: "s".to_string(),
            config_digest: "c".to_string(),
            toolchain: "rustc 1.96.0".to_string(),
            seeded_at_ms: 7,
            universe_seed_fingerprint: None,
        };
        write_manifest(&path, entries.iter(), Some(&record)).expect("write");

        let text = std::fs::read_to_string(&path).expect("read");
        let mut inventory = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            match serde_json::from_str::<p2_rig::ManifestLine>(line).expect("rig decode") {
                p2_rig::ManifestLine::Header { .. } => {}
                p2_rig::ManifestLine::Entry(e) => inventory.push((e.persist_id, e.cell, e.grid)),
            }
        }
        assert_eq!(
            inventory.len(),
            entries.len(),
            "the trailer must not land in the inventory, and no entry may be lost"
        );
        for ((pid, cell, grid), expected) in inventory.iter().zip(&entries) {
            assert_eq!(*pid, expected.persist_id);
            assert_eq!(*cell, expected.cell);
            assert_eq!(*grid, Some(expected.grid));
        }
    }

    #[test]
    fn emitted_manifest_is_jsonl_with_a_content_version_trailer() {
        // §9.3's shape, as a consumer sees it: one entry per line, each line
        // independently parseable (which is what "streams out" buys), and the
        // `content/version` tuple as the final line.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("manifest.json");
        let entries: Vec<_> = (0..64).map(entry).collect();
        let record = ContentVersion {
            content_build: "b".to_string(),
            manifest_digest: "deadbeef".to_string(),
            scenario_seed: "s".to_string(),
            config_digest: "c".to_string(),
            toolchain: "rustc 1.96.0".to_string(),
            seeded_at_ms: 7,
            universe_seed_fingerprint: None,
        };
        write_manifest(&path, entries.iter(), Some(&record)).expect("write");

        let text = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), entries.len() + 1, "64 entries plus a trailer");
        assert!(
            !text.trim_start().starts_with('['),
            "a JSON array cannot be read line by line — that was the bug"
        );

        // Every entry line round-trips on its own, in order, with no
        // preceding context.
        for (line, expected) in lines.iter().zip(&entries) {
            let decoded: ManifestEntry = serde_json::from_str(line).expect("entry line");
            assert_eq!(&decoded, expected);
        }

        // The trailer is the last line and carries the §9.3 tuple under a
        // `content_version` key, which is how the P2 rig tells it from an
        // entry.
        let trailer: serde_json::Value =
            serde_json::from_str(lines[lines.len() - 1]).expect("trailer line");
        let record_json = &trailer["content_version"];
        assert_eq!(record_json["manifest_digest"], "deadbeef");
        assert_eq!(record_json["content_build"], "b");
        assert!(
            serde_json::from_str::<ManifestEntry>(lines[lines.len() - 1]).is_err(),
            "the trailer must not decode as an entry, or a consumer would \
             count it as a seeded row"
        );
    }
}
