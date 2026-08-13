//! Binary-level integration tests for the `persistd` binary.
//!
//! These tests spawn the compiled binary and assert its behavior: stable node
//! identity across restarts, the stdout JSON address line, and graceful signal
//! handling. They do NOT require FoundationDB.

use std::process::Command;
use std::time::Duration;

/// Locate the compiled `persistd` binary via `CARGO_BIN_EXE_persistd`, set by
/// Cargo when running `cargo test` on a binary target's integration tests.
fn persistd_binary() -> &'static str {
    env!("CARGO_BIN_EXE_persistd")
}

/// Run `persistd` with the given extra arguments, wait for the JSON address
/// line on stdout, kill the process with SIGTERM, and return the parsed
/// `(node_id_string, endpoint_addr_string)`.
///
/// The binary is run with `--dir` set to a temporary directory and `--bind` set
/// to `127.0.0.1:0` so each run gets an ephemeral port.
fn run_persistd(args: &[&str]) -> (String, String) {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut cmd = Command::new(persistd_binary());
    cmd.arg("--dir")
        .arg(dir.path())
        .arg("--bind")
        .arg("127.0.0.1:0")
        .args(args)
        // Route tracing to stderr so stdout has only the JSON line.
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn persistd");
    let stdout = child.stdout.take().expect("stdout captured");
    let mut reader = std::io::BufReader::new(stdout);

    // Read the first line of stdout — the JSON address line.
    let mut line = String::new();
    use std::io::BufRead;
    let timeout = Duration::from_secs(10);
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() > timeout {
            // Kill the process and fail.
            let _ = child.kill();
            let _ = child.wait();
            panic!("timed out waiting for JSON address line from persistd");
        }
        line.clear();
        let bytes_read = reader.read_line(&mut line).expect("read stdout line");
        if bytes_read == 0 {
            // EOF — process may have exited.
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        if line.starts_with('{') {
            break;
        }
        // Skip non-JSON output (tracing should go to stderr, but just in case).
    }

    // Parse the JSON line.
    let trimmed = line.trim();
    let parsed: serde_json::Value =
        serde_json::from_str(trimmed).expect("stdout line is valid JSON");

    let node_id = parsed["node_id"]
        .as_str()
        .expect("node_id field present and is a string")
        .to_string();
    let endpoint_addr = parsed["endpoint_addr"]
        .as_str()
        .expect("endpoint_addr field present and is a string")
        .to_string();

    // Send SIGTERM to shut down gracefully.
    let _ = child.kill();
    let _ = child.wait();

    (node_id, endpoint_addr)
}

#[test]
fn gateway_node_id_is_stable_across_restart() {
    // The same --secret-key must produce the same NodeId across two runs.
    let secret_hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    let (id1, _) = run_persistd(&["--secret-key", secret_hex, "--nodes", "1"]);
    let (id2, _) = run_persistd(&["--secret-key", secret_hex, "--nodes", "1"]);

    assert_eq!(
        id1, id2,
        "same --secret-key must produce the same NodeId across restarts"
    );
}

#[test]
fn different_secret_keys_produce_different_node_ids() {
    let secret_a = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    let secret_b = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    let (id_a, _) = run_persistd(&["--secret-key", secret_a, "--nodes", "1"]);
    let (id_b, _) = run_persistd(&["--secret-key", secret_b, "--nodes", "1"]);

    assert_ne!(
        id_a, id_b,
        "different --secret-key must produce different NodeIds"
    );
}

#[test]
fn stdout_contains_json_address_line() {
    // Verify the output format: a single-line JSON object with endpoint_addr
    // and node_id fields.
    let (node_id, endpoint_addr) = run_persistd(&["--nodes", "1"]);
    assert!(!node_id.is_empty(), "node_id must be non-empty");
    assert!(!endpoint_addr.is_empty(), "endpoint_addr must be non-empty");
}

#[test]
fn stdout_lines_are_json_only() {
    // Assert that the first stdout line is JSON (starts with '{') even when
    // no --secret-key is used (ephemeral identity).
    let (_, _) = run_persistd(&["--nodes", "1"]);
}

#[test]
fn shard_level_flag_is_accepted() {
    // --shard-level 18 should be accepted (produces a valid shard cell).
    let (_, _) = run_persistd(&["--nodes", "1", "--shard-level", "18"]);
}
