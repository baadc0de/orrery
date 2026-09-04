//! The two-half digest (blake3 over the Unreal package and the collision packages, read back from
//! disk by this separate process), the flip-one-byte check, and raw/zstd sizes.

use std::error::Error;
use std::path::{Path, PathBuf};

fn file_digest(path: &Path) -> Result<(String, u64), Box<dyn Error>> {
    let bytes = std::fs::read(path)?;
    Ok((
        blake3::hash(&bytes).to_hex().to_string(),
        bytes.len() as u64,
    ))
}

/// Digest of a set of files in a fixed order: blake3 over (len, bytes) of each.
fn combined(paths: &[PathBuf]) -> Result<String, Box<dyn Error>> {
    let mut h = blake3::Hasher::new();
    for p in paths {
        let bytes = std::fs::read(p)?;
        h.update(&(bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    Ok(h.finalize().to_hex().to_string())
}

pub fn run(unreal: &Path, collision: &[&str], out: Option<&Path>) -> Result<(), Box<dyn Error>> {
    let mut halves: Vec<PathBuf> = vec![unreal.to_path_buf()];
    halves.extend(collision.iter().map(PathBuf::from));
    let mut per_file = serde_json::Map::new();
    for p in &halves {
        let (d, len) = file_digest(p)?;
        per_file.insert(
            p.display().to_string(),
            serde_json::json!({"blake3": d, "bytes": len}),
        );
    }
    let digest = combined(&halves)?;

    // Flip one byte of each half in a scratch copy and confirm the digest moves.
    let mut flips = Vec::new();
    for (idx, p) in halves.iter().enumerate() {
        let mut bytes = std::fs::read(p)?;
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0x01;
        let tmp = std::env::temp_dir().join(format!("onebody-flip-{idx}.bin"));
        std::fs::write(&tmp, &bytes)?;
        let mut alt = halves.clone();
        alt[idx].clone_from(&tmp);
        let flipped = combined(&alt)?;
        std::fs::remove_file(&tmp)?;
        flips.push(serde_json::json!({
            "half": p.display().to_string(),
            "byte_index": mid,
            "digest_after_flip": flipped,
            "changed": flipped != digest,
        }));
    }
    let all_changed = flips.iter().all(|f| f["changed"] == true);
    let report = serde_json::json!({
        "digest_blake3": digest,
        "halves": per_file,
        "flip_one_byte": flips,
        "flip_check_passed": all_changed,
    });
    let text = serde_json::to_string_pretty(&report)?;
    if let Some(out) = out {
        std::fs::write(out, &text)?;
    }
    println!("{text}");
    if all_changed {
        Ok(())
    } else {
        Err("flip-one-byte check failed".into())
    }
}

/// Raw and `zstd -19` sizes (the transfer proxy spike 4 multiplies), via the zstd CLI when present.
pub fn sizes(files: &[&str], out: Option<&Path>) -> Result<(), Box<dyn Error>> {
    let mut rows = Vec::new();
    for f in files {
        let raw = std::fs::metadata(f)?.len();
        let zstd = std::process::Command::new("zstd")
            .args(["-19", "-q", "-c", f])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| o.stdout.len() as u64);
        rows.push(serde_json::json!({"file": f, "bytes": raw, "zstd19_bytes": zstd}));
    }
    let text = serde_json::to_string_pretty(&rows)?;
    if let Some(out) = out {
        std::fs::write(out, &text)?;
    }
    println!("{text}");
    Ok(())
}
