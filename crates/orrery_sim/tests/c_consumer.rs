use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use glam::IVec3;
use orrery_core::{CoreCodec, QPos};
use orrery_games::regolith::archetype::Archetype;
use orrery_games::regolith::state::{Craft, RegolithState};
use orrery_protocol::channels::encode_replication;
use orrery_protocol::{CellId, PersistId, INTEREST_LEVEL};

struct CExamplePaths {
    fixture: PathBuf,
    executable: PathBuf,
    library_dir: PathBuf,
}

#[test]
fn a_c_caller_steps_the_simulation_without_linking_rust_types() {
    let paths = prepare_c_example();
    build_cdylib(&paths.library_dir);
    compile_c_example(&paths);

    let output = Command::new(&paths.executable)
        .arg(&paths.fixture)
        .output()
        .expect("run compiled C example");
    assert!(
        output.status.success(),
        "C example failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("C output is UTF-8"),
        "craft id=42 position_mm=(123400, -5600, 78000) yaw_urad=1570796 pitch_urad=0\n"
    );
}

fn prepare_c_example() -> CExamplePaths {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_dir = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("crate has workspace parent");
    let test_dir = env::temp_dir().join(format!("orrery-sim-c-example-{}", std::process::id()));
    fs::create_dir_all(&test_dir).expect("create C example directory");

    let craft = Craft::spawned(
        Archetype::Interceptor,
        QPos {
            x: 123_400,
            y: -5_600,
            z: 78_000,
        },
        1_570_796,
    );
    let state = RegolithState::Craft(craft).to_canonical();
    let cell = CellId::from_coords(IVec3::ZERO, INTEREST_LEVEL).expect("origin cell is valid");
    let fixture = test_dir.join("live-campaign.replication");
    fs::write(
        &fixture,
        encode_replication(&(state, cell, PersistId::new(42), 17_u64)),
    )
    .expect("write replication fixture");

    let library_dir = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_dir.join("target"))
        .join("release");
    CExamplePaths {
        fixture,
        executable: test_dir.join("live_campaign"),
        library_dir,
    }
}

fn build_cdylib(library_dir: &Path) {
    let status = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["build", "--release", "-p", "orrery_sim"])
        .status()
        .expect("run cargo build for C cdylib");
    assert!(status.success(), "build the C cdylib");
    assert!(
        library_dir.join("liborrery_sim.so").is_file(),
        "cdylib exists"
    );
}

fn compile_c_example(paths: &CExamplePaths) {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let compiler = env::var_os("CC").unwrap_or_else(|| "cc".into());
    let rpath = format!("-Wl,-rpath,{}", paths.library_dir.display());
    let status = Command::new(compiler)
        .arg("-std=c11")
        .arg("-Wall")
        .arg("-Wextra")
        .arg("-Werror")
        .arg("-I")
        .arg(manifest_dir.join("include"))
        .arg(manifest_dir.join("examples/c/live_campaign.c"))
        .arg("-L")
        .arg(&paths.library_dir)
        .arg("-lorrery_sim")
        .arg(rpath)
        .arg("-o")
        .arg(&paths.executable)
        .status()
        .expect("compile C example");
    assert!(status.success(), "compile C example with a C compiler");
}
