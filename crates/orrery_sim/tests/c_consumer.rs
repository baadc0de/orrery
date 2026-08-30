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

/// A C caller creates the mirror, applies one replication datagram and reads
/// a transform back, using nothing but the header. This deliberately does NOT
/// cover `orrery_sim_step`; the test below owns that, because a name that
/// claims stepping while never calling it is worse than no name at all.
#[test]
fn a_c_caller_drives_the_abi_without_linking_rust_types() {
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

/// Stepping must advance the mirror, and the ABI must be how a caller does it.
///
/// The rename above exposed the hole: the C example never calls
/// `orrery_sim_step`, so turning `step` into a no-op left every test green.
/// This asserts the property rather than a copied constant -- a craft carrying
/// positive x velocity must have a larger x after stepping than before, which
/// is false for any no-op and cannot be satisfied by echoing the input.
#[test]
fn stepping_advances_a_moving_craft_through_the_abi() {
    use orrery_core::QVel;

    let mut craft = Craft::spawned(Archetype::Interceptor, QPos { x: 0, y: 0, z: 0 }, 0);
    craft.vel = QVel {
        x: 40_000,
        y: 0,
        z: 0,
    };
    let entity = PersistId::new(7);
    let cell = CellId::from_coords(IVec3::ZERO, INTEREST_LEVEL).expect("origin cell is valid");
    let datagram = encode_replication(&(
        RegolithState::Craft(craft).to_canonical(),
        cell,
        entity,
        0_u64,
    ));

    let mut sim: *mut orrery_sim::OrrerySim = std::ptr::null_mut();
    unsafe {
        assert_eq!(
            orrery_sim::orrery_sim_create(&mut sim),
            orrery_sim::OrrerySimResult::Ok
        );
        assert_eq!(
            orrery_sim::orrery_sim_apply_replication(sim, datagram.as_ptr(), datagram.len()),
            orrery_sim::OrrerySimResult::Ok
        );
        let before = read_one(sim);
        assert_eq!(
            orrery_sim::orrery_sim_step(sim, 60),
            orrery_sim::OrrerySimResult::Ok
        );
        let after = read_one(sim);
        assert!(
            after.x_mm > before.x_mm,
            "a craft with positive x velocity must advance: before {} after {}",
            before.x_mm,
            after.x_mm
        );
        assert_eq!(
            orrery_sim::orrery_sim_destroy(sim),
            orrery_sim::OrrerySimResult::Ok
        );
    }
}

unsafe fn read_one(sim: *mut orrery_sim::OrrerySim) -> orrery_sim::OrrerySimCraftTransform {
    let mut out = [orrery_sim::OrrerySimCraftTransform {
        craft_id: 0,
        x_mm: 0,
        y_mm: 0,
        z_mm: 0,
        yaw_urad: 0,
        pitch_urad: 0,
    }; 4];
    let mut written: usize = 0;
    assert_eq!(
        orrery_sim::orrery_sim_copy_craft_transforms(
            sim,
            out.as_mut_ptr(),
            out.len(),
            &mut written
        ),
        orrery_sim::OrrerySimResult::Ok
    );
    assert_eq!(written, 1, "the fixture holds exactly one craft");
    out[0]
}
