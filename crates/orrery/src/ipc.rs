//! The IPC extraction crossing (#898 step 3): presentation frames out of the
//! live world, in `orrery_ipc`'s vocabulary.
//!
//! #898 step 2 landed the schema (`FrameBatch`, `EntityFrame`,
//! `SpawnBatch`, `DespawnBatch`, `CorrectionBatch`) and #967 the byte-stream
//! transport — but nothing produced a `FrameBatch` from a real Bevy world.
//! The transport's bench synthesizes its frames; two verification lanes
//! recorded the same finding: *the contract exists, the extractor does not.*
//! This module is the producer.
//!
//! # Why this module exists, and why it is here
//!
//! A9 §2.4 states the extraction contract and §4.2 its outbound shape, and
//! A9 §5's component 3 consumes it — but the producer cannot sit in any
//! member crate. `orrery_ipc` is Bevy-free by mechanical gate
//! (`core-gates.sh`'s `DECLARED_BEVY_FREE_CRATES`) and must not learn about
//! ECS; `orrery_predict` must not learn what an IPC frame is; and the
//! lightyear markers that identify the predicted and interpolated classes are
//! lightyear types, which nothing outside `orrery_predict`'s internals may
//! name (docs/10-crates.md layering rule 3, the plan-B seam). So the
//! extraction runs here, under the same rule as every other crossing in this
//! crate: it moves a settled value across a seam and decides nothing beyond
//! *whether* the move applies.
//!
//! The plan-B seam is respected by making the markers the *game's* choice:
//! the system is generic over two marker components, and a lightyear game
//! instantiates it with lightyear's own `Predicted` and `Interpolated`
//! markers — which the game already names when it registers prediction. The
//! facade never does. Replacing lightyear rewrites `orrery_predict`'s
//! internals and changes two generic arguments at the call site; it does not
//! touch this module.
//!
//! # What it extracts
//!
//! One `SidecarToEngine::Frames` batch per run, plus `Spawns`, `Despawns`
//! and `Corrections` batches when they are non-empty:
//!
//! - **Predicted entities** are those carrying the `P` marker, the game's
//!   presented component `C`, and a [`PersistIdentity`]. Their basis is
//!   [`InterpBasis::exact`] at the extraction tick, which is what the schema
//!   prescribes for a predicted sample.
//! - **Interpolated entities** are those carrying the `I` marker, `C`, a
//!   [`PersistIdentity`], and `orrery_predict`'s [`RenderedInterpBasis`] —
//!   the basis `orrery_predict` co-produces with the presented value. The
//!   basis is *read*, never reconstructed: lightyear exposes no record of the
//!   bracket its own apply phase selected, which is exactly why
//!   [`RenderedInterpBasis`] exists, and a basis recomputed beside the frame
//!   could disagree with the value it claims to describe. Requiring the
//!   component means an entity presented on lightyear's own apply path —
//!   which has no honest basis to export — is not exported at all rather than
//!   exported with an invented one.
//!
//! The transform is the game's projection of its presented component:
//! [`PresentationFrame`]. The projection is the game's for the same reason
//! `ReconciliationResidual` is — only the game knows which of its components
//! carries position and in what units, so the projection onto the
//! millimetre lattice is its to define. It is a trait on the component
//! rather than a blanket impl on Bevy's `Transform` because an impl on a
//! foreign type is an orphan violation in every crate that could write it.
//!
//! # Why the batch is a message
//!
//! [`IpcOutbound`] is an ECS message carrying the schema's own
//! [`SidecarToEngine`] enum. The consumer is the sidecar's IPC transport,
//! which encodes each batch and writes it to whatever link the observer is
//! on; the message is the seam that keeps this crate from naming a socket,
//! and the transport from naming a query.
//!
//! # The spawn/despawn and correction cursors
//!
//! Bevy change detection cannot produce these batches directly. A despawn
//! removes the entity, so `RemovedComponents` cannot read its
//! [`PersistIdentity`]; and `Added<…>` edges fire again when lightyear's
//! rollback re-adds components it restored, which would fabricate a
//! spawn/despawn pair for an entity that was presented the whole time. Both
//! batches are therefore diffs of the presentation set the extractor itself
//! observed — bookkeeping about presentation, which is the extractor's own
//! cursor (A9 §2.4: copy-out, overwrite semantics), not canonical state.
//!
//! Corrections are sourced without inventing any new state. lightyear fires
//! no rollback signal (see `orrery_predict`'s wiring docs), but
//! `orrery_predict::feed_residuals` already counts one `rollbacks` increment
//! per `Added<VisualCorrection<D>>` it observes, on
//! [`TrackKey`](orrery_predict::TrackKey)s derivable from the world's
//! `PredictedBy` components. The extractor diffs those counters: an increase
//! is one newly observed correction, stamped with the tick the residual
//! recorded. A key is baselined on first sight rather than replayed, so an
//! extractor that joins mid-session does not re-emit corrections it never
//! saw; a decreased counter (the monitor's reset) re-baselines silently.
//!
//! # Ordering
//!
//! The system runs in `Update`, after
//! [`PredictSystems::ProduceInterpolatedFrame`] — the set the presented
//! value/basis pair is co-produced in — so a batch never carries a stale
//! frame beside a fresh basis or the reverse. Rollback replays run inside
//! `FixedMain`; by the time this system runs, the presented components hold
//! the replayed, rules-produced values, so the next batch *is* the
//! regenerated presentation A9 §2.4(4) asks for — overwrite semantics by
//! construction, with no undo logic here or in the consumer.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::marker::PhantomData;

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

use orrery_authority::PersistIdentity;
use orrery_ipc::{
    CorrectionBatch, CorrectionNotice, DespawnBatch, EntityFrame, FrameBatch, QuantizedTransform,
    SidecarToEngine, SpawnBatch,
};
use orrery_predict::{
    PredictSystems, PredictedBy, ReconciliationMonitor, RenderedInterpBasis, TickBridge,
};
use orrery_protocol::{InterpBasis, PersistId};

/// The game's projection from its presented component onto the frame's
/// quantized transform.
///
/// Implemented for the component the game registered for prediction and
/// interpolation — the one whose value is actually presented. Translation is
/// grid-relative millimetres; the forward and up directions must be non-zero
/// and non-collinear, per `orrery_ipc`'s `QuantizedTransform` contract.
pub trait PresentationFrame: Component {
    /// The quantized transform this presented value projects onto.
    fn frame(&self) -> QuantizedTransform;
}

/// One extracted batch, in the schema's own vocabulary.
///
/// Written by [`export_ipc_frames`] every run — the frames batch always, the
/// other three only when non-empty — and consumed by the sidecar's IPC
/// transport. Overwrite semantics: a consumer applies the newest batch and
/// holds no history to unwind (A9 §2.4(3)).
#[derive(Debug, Clone, PartialEq, Eq, Message)]
pub struct IpcOutbound(pub SidecarToEngine);

/// What the extractor observed on its previous run.
///
/// One per extraction plugin instance, keyed on the presented component —
/// two presented components are two presentation streams, each with its own
/// membership. This is presentation-side bookkeeping about presentation and
/// nothing else: it holds no canonical state and survives no rollback
/// reconstruction (see the module docs for why a diff cursor is used instead
/// of change detection).
#[derive(Resource)]
pub struct ExtractionCursor<C: PresentationFrame> {
    presented: BTreeSet<PersistId>,
    corrections: HashMap<orrery_predict::TrackKey, u32>,
    marker: PhantomData<fn() -> C>,
}

impl<C: PresentationFrame> Default for ExtractionCursor<C> {
    fn default() -> Self {
        Self {
            presented: BTreeSet::new(),
            corrections: HashMap::new(),
            marker: PhantomData,
        }
    }
}

/// The predicted half of the extraction query: identity and presented value,
/// on an entity carrying the predicted marker and not the interpolated one.
type PredictedFrames<'w, 's, P, I, C> =
    Query<'w, 's, (&'static PersistIdentity, &'static C), (With<P>, Without<I>)>;

/// The interpolated half: identity, presented value, and the basis the
/// pipeline exported with it.
type InterpolatedFrames<'w, 's, I, C> = Query<
    'w,
    's,
    (
        &'static PersistIdentity,
        &'static C,
        &'static RenderedInterpBasis,
    ),
    With<I>,
>;

/// Extracts one [`FrameBatch`] from the live world, plus the spawn, despawn
/// and correction batches the run's diff produces.
///
/// See the [module docs](self) for the contract, the cursor rationale, and
/// the ordering constraint. The extraction tick is the universe tick the
/// [`TickBridge`] last advanced to, resolved through it — the tick the
/// presented values were produced on, without naming a lightyear type.
pub fn export_ipc_frames<P, I, C>(
    bridge: Res<TickBridge>,
    monitor: Res<ReconciliationMonitor>,
    predicted: PredictedFrames<P, I, C>,
    interpolated: InterpolatedFrames<I, C>,
    attributed: Query<&PredictedBy>,
    mut out: MessageWriter<IpcOutbound>,
    mut cursor: ResMut<ExtractionCursor<C>>,
) where
    P: Component,
    I: Component,
    C: PresentationFrame,
{
    let extracted_at = bridge.resolve(bridge.last_seen());

    let mut predicted_frames = Vec::new();
    let mut presented_now = BTreeSet::new();
    for (identity, value) in &predicted {
        predicted_frames.push(EntityFrame {
            persist_id: identity.0,
            transform: value.frame(),
            basis: InterpBasis::exact(extracted_at),
        });
        presented_now.insert(identity.0);
    }

    let mut interpolated_frames = Vec::new();
    for (identity, value, basis) in &interpolated {
        interpolated_frames.push(EntityFrame {
            persist_id: identity.0,
            transform: value.frame(),
            basis: basis.0,
        });
        presented_now.insert(identity.0);
    }

    let mut spawns = Vec::new();
    let mut despawns = Vec::new();
    for id in presented_now.difference(&cursor.presented) {
        spawns.push(*id);
    }
    for id in cursor.presented.difference(&presented_now) {
        despawns.push(*id);
    }
    cursor.presented = presented_now;

    let mut notices = Vec::new();
    let mut keys_now = HashSet::new();
    for attributed_by in &attributed {
        let key = attributed_by.track_key();
        keys_now.insert(key);
        let count = monitor.track(&key).map_or(0, |track| track.rollbacks);
        match cursor.corrections.get(&key) {
            // First sight baselines rather than replays: the extractor that
            // joins mid-session did not observe the corrections the counter
            // already accumulated.
            None => {
                cursor.corrections.insert(key, count);
            }
            Some(seen) if count > *seen => {
                let observed_at = monitor
                    .track(&key)
                    .and_then(|track| track.last_tick)
                    .unwrap_or(extracted_at);
                notices.push(CorrectionNotice {
                    persist_id: key.entity,
                    observed_at,
                });
                cursor.corrections.insert(key, count);
            }
            // The monitor's counters were reset: re-baseline, emitting
            // nothing, so the next real correction is one increment again.
            Some(seen) if count < *seen => {
                cursor.corrections.insert(key, count);
            }
            Some(_) => {}
        }
    }
    // A `PredictedBy` that left the world ends its diff entry; if the
    // attribution returns, first sight baselines again.
    cursor.corrections.retain(|key, _| keys_now.contains(key));

    out.write(IpcOutbound(SidecarToEngine::Frames(FrameBatch {
        extracted_at,
        predicted: predicted_frames,
        interpolated: interpolated_frames,
    })));
    if !spawns.is_empty() {
        out.write(IpcOutbound(SidecarToEngine::Spawns(SpawnBatch {
            entities: spawns,
        })));
    }
    if !despawns.is_empty() {
        out.write(IpcOutbound(SidecarToEngine::Despawns(DespawnBatch {
            entities: despawns,
        })));
    }
    if !notices.is_empty() {
        out.write(IpcOutbound(SidecarToEngine::Corrections(CorrectionBatch {
            corrections: notices,
        })));
    }
}

/// The plugin's unused generic parameters. `fn() -> (P, I, C)` keeps it
/// `Send + Sync` and invariant-free regardless of the parameters.
type ExportMarkers<P, I, C> = PhantomData<fn() -> (P, I, C)>;

/// Installs [`export_ipc_frames`] for one presented component.
///
/// Generic over the predicted marker `P`, the interpolated marker `I`, and
/// the presented component `C` — the game's instantiation of the first two
/// is what keeps lightyear types out of this crate. Like
/// [`OrreryHitRegistrationPlugin`](crate::hit::OrreryHitRegistrationPlugin),
/// it is deliberately *not* a member of [`OrreryClientPlugins`]: it is
/// generic over game-declared types, which the group cannot invent. Add it
/// after the group.
///
/// [`OrreryClientPlugins`]: crate::OrreryClientPlugins
#[derive(Debug, Default, Clone, Copy)]
pub struct OrreryIpcExportPlugin<P, I, C> {
    marker: ExportMarkers<P, I, C>,
}

impl<P, I, C> OrreryIpcExportPlugin<P, I, C> {
    /// The plugin for the given markers and presented component.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            marker: PhantomData,
        }
    }
}

impl<P: Component, I: Component, C: PresentationFrame> Plugin for OrreryIpcExportPlugin<P, I, C> {
    fn build(&self, app: &mut App) {
        app.add_message::<IpcOutbound>();
        app.init_resource::<ExtractionCursor<C>>();
        app.add_systems(
            Update,
            export_ipc_frames::<P, I, C>.after(PredictSystems::ProduceInterpolatedFrame),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ecs::system::RunSystemOnce;
    use orrery_protocol::{LatticePoint, PersistId, QuantizedDir, Tick};

    /// One presented component per extraction stream; the cursor is keyed
    /// on it, so two streams keep two memberships.
    #[derive(Debug, Clone, Copy, PartialEq, Component)]
    struct Presented(i64);

    impl PresentationFrame for Presented {
        fn frame(&self) -> QuantizedTransform {
            QuantizedTransform {
                translation: LatticePoint::new(self.0, 0, 0),
                forward: QuantizedDir::new(1, 0, 0),
                up: QuantizedDir::new(0, 1, 0),
            }
        }
    }

    #[derive(Component)]
    struct PredictedMarker;
    #[derive(Component)]
    struct InterpolatedMarker;

    const A: PersistId = PersistId::new(1);
    const B: PersistId = PersistId::new(2);

    /// One extraction run over `world`, returning exactly the batches that
    /// run emitted — the message buffer is flipped afterwards, so the next
    /// call cannot see them again.
    fn batches(world: &mut World) -> Vec<SidecarToEngine> {
        world
            .run_system_once(export_ipc_frames::<PredictedMarker, InterpolatedMarker, Presented>)
            .expect("extraction runs");
        let emitted: Vec<SidecarToEngine> = world
            .resource::<Messages<IpcOutbound>>()
            .iter_current_update_messages()
            .map(|outbound| outbound.0.clone())
            .collect();
        world.resource_mut::<Messages<IpcOutbound>>().update();
        emitted
    }

    fn frames(world: &mut World) -> FrameBatch {
        batches(world)
            .into_iter()
            .find_map(|batch| match batch {
                SidecarToEngine::Frames(batch) => Some(batch),
                _ => None,
            })
            .expect("one frames batch per run")
    }

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(TickBridge::anchor(Tick(1_000), 0));
        world.insert_resource(ReconciliationMonitor::default());
        world.init_resource::<Messages<IpcOutbound>>();
        world.init_resource::<ExtractionCursor<Presented>>();
        world
    }

    #[test]
    fn a_predicted_entity_is_extracted_with_an_exact_basis_at_the_bridge_tick() {
        let mut world = world();
        world.spawn((PersistIdentity(A), Presented(41), PredictedMarker));

        let batch = frames(&mut world);
        assert_eq!(batch.extracted_at, Tick(1_000), "bridge-resolved tick");
        assert_eq!(batch.predicted.len(), 1);
        assert_eq!(batch.interpolated, Vec::new());
        let frame = &batch.predicted[0];
        assert_eq!(frame.persist_id, A);
        assert_eq!(frame.transform.translation.x, 41, "the presented value");
        assert_eq!(
            frame.basis,
            InterpBasis::exact(Tick(1_000)),
            "a predicted sample names one tick exactly"
        );
    }

    #[test]
    fn membership_diffs_produce_spawn_and_despawn_batches_exactly_once() {
        let mut world = world();
        let a = world
            .spawn((PersistIdentity(A), Presented(1), PredictedMarker))
            .id();
        world.spawn((PersistIdentity(B), Presented(2), PredictedMarker));

        let first = batches(&mut world);
        assert!(
            matches!(
                first
                    .iter()
                    .find(|batch| matches!(batch, SidecarToEngine::Spawns(_))),
                Some(SidecarToEngine::Spawns(SpawnBatch { entities }))
                    if entities.len() == 2,
            ),
            "both ids enter presentation once: {first:?}"
        );

        let quiet = batches(&mut world);
        assert!(
            !quiet
                .iter()
                .any(|batch| !matches!(batch, SidecarToEngine::Frames(_))),
            "an unchanged presentation set emits only frames: {quiet:?}"
        );

        world.despawn(a);
        let after = batches(&mut world);
        assert!(
            matches!(
                after
                    .iter()
                    .find(|batch| matches!(batch, SidecarToEngine::Despawns(_))),
                Some(SidecarToEngine::Despawns(DespawnBatch { entities }))
                    if entities == &vec![A],
            ),
            "the id that left presentation is named by the despawn batch: {after:?}"
        );
        let batch = frames(&mut world);
        assert!(
            batch.predicted.iter().all(|frame| frame.persist_id != A),
            "a despawned entity is not framed again"
        );
    }

    #[test]
    fn correction_counters_diff_into_notices_without_replaying_or_repeating() {
        let mut world = world();
        let authority = iroh::SecretKey::from_bytes(&[7_u8; 32]).public();
        let entity = world
            .spawn((
                PersistIdentity(A),
                Presented(1),
                PredictedMarker,
                PredictedBy {
                    authority,
                    persist_id: A,
                },
            ))
            .id();
        let key = orrery_predict::TrackKey {
            authority,
            entity: A,
        };
        let correct = |world: &mut World, tick: u64, error: i64| {
            let mut monitor = world.resource_mut::<ReconciliationMonitor>();
            monitor.record_rollback(key);
            monitor.record_residual(key, Tick(tick), error, 0);
        };

        // First sight baselines, even though corrections already happened
        // before this extractor started observing.
        correct(&mut world, 1_002, 41);
        assert!(
            batches(&mut world)
                .iter()
                .all(|batch| !matches!(batch, SidecarToEngine::Corrections(_))),
            "first sight must not replay corrections it never observed"
        );

        // One increment since the baseline: one notice, stamped with the
        // tick the residual recorded, and not repeated on the next run.
        correct(&mut world, 1_003, 42);
        let with_notice = batches(&mut world);
        assert!(
            matches!(
                with_notice
                    .iter()
                    .find(|batch| matches!(batch, SidecarToEngine::Corrections(_))),
                Some(SidecarToEngine::Corrections(CorrectionBatch { corrections }))
                    if corrections == &vec![CorrectionNotice { persist_id: A, observed_at: Tick(1_003) }],
            ),
            "the increment becomes one notice at the residual's tick: {with_notice:?}"
        );
        assert!(
            batches(&mut world)
                .iter()
                .all(|batch| !matches!(batch, SidecarToEngine::Corrections(_))),
            "a quiet run emits no corrections"
        );

        // An attributed entity that leaves the world drops its diff entry.
        world.despawn(entity);
        let _ = batches(&mut world);
        assert_eq!(
            world
                .resource::<ExtractionCursor<Presented>>()
                .corrections
                .len(),
            0,
            "a departed attribution must not pin its cursor entry forever"
        );
    }
}
