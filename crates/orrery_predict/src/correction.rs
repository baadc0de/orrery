//! Ingress from signed adjudication into the ordinary reconciliation path.

use std::collections::VecDeque;
use std::sync::Arc;

use bevy_ecs::prelude::*;
use lightyear::prelude::LocalTimeline;
use orrery_protocol::{AuthorityCorrectionClaimsV1, Tick};

use crate::{PredictConfig, TickBridge};

/// How the normal authoritative-update path must consume corrected state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorityCorrectionPlan {
    /// The authoritative tick remains buffered: install it and replay forward.
    Rollback {
        /// The authoritative state tick to restore before replay.
        tick: Tick,
    },
    /// The tick has left the ring: snap simulation state and smooth presentation.
    Snap {
        /// The authoritative state tick represented by the snapshot.
        tick: Tick,
    },
}

/// The game's existing authoritative-state reconciliation ingress.
///
/// Implementations must feed the same component/timeline path ordinary
/// replicated authoritative updates use. This trait is an adapter to that
/// game-specific path, not a second rollback implementation: Orrery decides
/// only rollback-versus-snap under D8's window and hands over canonical state.
pub trait AuthorityCorrectionReconciler: Send + Sync + 'static {
    /// Install and reconcile one already-signature-verified correction.
    fn reconcile(
        &self,
        correction: &AuthorityCorrectionClaimsV1,
        plan: AuthorityCorrectionPlan,
    ) -> Result<(), String>;
}

/// Shared game adapter for [`AuthorityCorrectionReconciler`].
#[derive(Resource, Clone)]
pub struct SharedAuthorityCorrectionReconciler(pub Arc<dyn AuthorityCorrectionReconciler>);

/// Verified corrections waiting for the normal reconciliation adapter.
#[derive(Debug, Default, Resource)]
pub struct AuthorityCorrectionInbox(VecDeque<AuthorityCorrectionClaimsV1>);

impl AuthorityCorrectionInbox {
    /// Queue one correction already verified by `orrery_persist_client`.
    pub fn push(&mut self, correction: AuthorityCorrectionClaimsV1) {
        self.0.push_back(correction);
    }

    /// Number awaiting reconciliation.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether none awaits reconciliation.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// D8's ordinary window decision for one authoritative state update.
#[must_use]
pub const fn authority_correction_plan(
    now: Tick,
    authoritative_tick: Tick,
    rollback_ticks: u16,
) -> AuthorityCorrectionPlan {
    if now.0.saturating_sub(authoritative_tick.0) <= rollback_ticks as u64 {
        AuthorityCorrectionPlan::Rollback {
            tick: authoritative_tick,
        }
    } else {
        AuthorityCorrectionPlan::Snap {
            tick: authoritative_tick,
        }
    }
}

fn dispatch_authority_correction(
    reconciler: &dyn AuthorityCorrectionReconciler,
    correction: &AuthorityCorrectionClaimsV1,
    now: Tick,
    rollback_ticks: u16,
) -> Result<(), String> {
    reconciler.reconcile(
        correction,
        authority_correction_plan(now, correction.authoritative_tick, rollback_ticks),
    )
}

/// Drain verified corrections into the game's normal reconciliation ingress.
pub fn reconcile_authority_corrections(
    mut inbox: ResMut<AuthorityCorrectionInbox>,
    reconciler: Option<Res<SharedAuthorityCorrectionReconciler>>,
    config: Res<PredictConfig>,
    timeline: Res<LocalTimeline>,
    bridge: Res<TickBridge>,
) {
    let Some(reconciler) = reconciler else {
        // Keep the correction queued. Dropping it because the game forgot its
        // CoreState adapter would turn a configuration defect into a silent
        // security no-op.
        return;
    };
    let now = bridge.resolve(timeline.tick().0);
    while let Some(correction) = inbox.0.pop_front() {
        if let Err(error) = dispatch_authority_correction(
            reconciler.0.as_ref(),
            &correction,
            now,
            config.rollback_ticks,
        ) {
            tracing::warn!(%error, entity = ?correction.entity, "authority correction reconciliation failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::{NodeId, PersistId, RulesetId};
    use std::sync::Mutex;

    #[test]
    fn an_old_adjudication_snaps_instead_of_silently_missing_the_ring() {
        assert_eq!(
            authority_correction_plan(Tick::new(200), Tick::new(190), 9),
            AuthorityCorrectionPlan::Snap {
                tick: Tick::new(190)
            }
        );
        assert_eq!(
            authority_correction_plan(Tick::new(200), Tick::new(191), 9),
            AuthorityCorrectionPlan::Rollback {
                tick: Tick::new(191)
            }
        );
    }

    struct OrdinaryPath {
        state: Mutex<Vec<u8>>,
        plan: Mutex<Option<AuthorityCorrectionPlan>>,
    }

    impl AuthorityCorrectionReconciler for OrdinaryPath {
        fn reconcile(
            &self,
            correction: &AuthorityCorrectionClaimsV1,
            plan: AuthorityCorrectionPlan,
        ) -> Result<(), String> {
            *self.state.lock().expect("state") = correction.authoritative_state.clone();
            *self.plan.lock().expect("plan") = Some(plan);
            Ok(())
        }
    }

    #[test]
    fn correction_enters_the_ordinary_path_and_changes_resulting_state() {
        let ordinary = OrdinaryPath {
            state: Mutex::new(b"predicted".to_vec()),
            plan: Mutex::new(None),
        };
        let correction = AuthorityCorrectionClaimsV1 {
            issuer: NodeId::from_bytes(&[0; 32]).expect("valid test node"),
            subject: NodeId::from_bytes(&[1; 32]).expect("valid test node"),
            entity: PersistId::new(7),
            reconcile_from: Tick::new(92),
            authoritative_tick: Tick::new(95),
            authoritative_state: b"adjudicated".to_vec(),
            ruleset: RulesetId {
                version: 1,
                digest: [3; 32],
            },
            adjudication: [4; 32],
        };

        dispatch_authority_correction(&ordinary, &correction, Tick::new(100), 9)
            .expect("ordinary reconciliation accepts the correction");

        assert_eq!(*ordinary.state.lock().expect("state"), b"adjudicated");
        assert_eq!(
            *ordinary.plan.lock().expect("plan"),
            Some(AuthorityCorrectionPlan::Rollback {
                tick: Tick::new(95)
            })
        );
    }
}
