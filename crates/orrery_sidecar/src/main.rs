//! The shipped sidecar binary: canonical rules in Lightyear's tick, poses in
//! the authority's ring, verdicts on the wire.
//!
//! `orrery-sidecar` is the repository's first shipped binary that builds a
//! Bevy `App` over the client facade. Before it, every consumer of
//! `OrreryClientPlugins` — and so of the pose ring hit claims are validated
//! against — was a test or an example, which is the precise failure #871
//! names: a fully designed mechanism with no production caller.

use bevy::prelude::AppExit;
use orrery_protocol::PersistId;
use orrery_sidecar::{secret, sidecar, spawn_predicted};

/// The node seed. Deterministic so a scenario's node ids are reproducible;
/// a real deployment supplies its own key.
const NODE_SEED: u8 = 9;

/// The one entity this sidecar simulates and holds.
const ENTITY: PersistId = PersistId::new(1);

fn main() -> AppExit {
    let key = secret(NODE_SEED);
    let authority = key.public();
    let mut app = sidecar(key, true);
    spawn_predicted(&mut app, authority, ENTITY);
    app.run()
}
