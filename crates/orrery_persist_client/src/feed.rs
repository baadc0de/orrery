//! Replicon change-detection wiring: turn replicon `ComponentDiff`s into
//! [`DiffUplink`](orrery_protocol::DiffUplink)s for the uplink scheduler (D11
//! §2.1, docs/03-replication.md §5.3).
//!
//! The vendored replicon `uplink` feature emits a [`ComponentDiff`] message per
//! changed replicated component each tick (owner-side). The [`feed_uplink`]
//! system drains those and, for locally-authoritative entities that carry a
//! stable [`PersistId`], queues a `DiffUplink` into the [`UplinkScheduler`].
//!
//! [`PersistId`] is the canonical Bevy `Entity` ↔ persistent-id mapping on every
//! peer — a replicated component written only by the entity's owner and
//! maintained by this crate (docs/08-persistence.md). The [`LocallyAuthoritative`]
//! marker opts an entity into the uplink: only the peer that owns an entity may
//! write its durable state (single-writer per entity, D11).

use bevy_ecs::prelude::*;
use bevy_replicon::server::uplink::ComponentDiff;
use orrery_protocol::{DiffUplink, GridId, RecordKind, Tick};
use orrery_spatial::plugin::Cell;

pub use orrery_authority::LocallyAuthoritative;
use orrery_authority::{Authority, AuthorityPhase};

use crate::config::PersistClientConfig;
use crate::uplink::UplinkScheduler;

/// The canonical Bevy `Entity` ↔ persistent-id mapping (D11).
///
/// A replicated component, written only by the entity's owner. This is the id
/// that `DiffUplink`s (and thus journal records) are addressed by — never a Bevy
/// `Entity`, which is not stable across peers or restarts.
#[derive(Debug, Clone, Copy, Component)]
pub struct PersistId(pub orrery_protocol::PersistId);

impl PersistId {
    /// A component from a raw persistent id.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(orrery_protocol::PersistId::new(id))
    }
}

/// Per-entity diff sequence, for idempotent `(entity, tick)`-keyed records.
#[derive(Debug, Default, Resource)]
pub struct UplinkSeq {
    /// The next diff sequence per entity.
    pub next: std::collections::HashMap<Entity, u64>,
}

/// Feeds replicon change-detection diffs into the [`UplinkScheduler`].
///
/// Runs after replicon's `collect_uplink_diffs` each tick. For every
/// [`ComponentDiff`] whose entity is locally-authoritative and carries a
/// [`PersistId`] and a [`Cell`], builds a [`DiffUplink`] and queues it for the
/// scheduler's next flush.
pub fn feed_uplink(
    cfg: Res<PersistClientConfig>,
    mut scheduler: ResMut<UplinkScheduler>,
    mut seq: ResMut<UplinkSeq>,
    mut diffs: MessageReader<ComponentDiff>,
    entities: Query<(Entity, &PersistId, &Cell, &Authority, &AuthorityPhase)>,
    authorities: Query<(), With<LocallyAuthoritative>>,
) {
    for diff in diffs.read() {
        // Only locally-authoritative entities are ours to persist.
        if authorities.get(diff.entity).is_err() {
            continue;
        }
        let Ok((entity, persist_id, cell, authority, phase)) = entities.get(diff.entity) else {
            continue;
        };
        let AuthorityPhase::LocalGranted { lease_id, .. } = *phase else {
            continue;
        };

        // Register at the fastest uplink rate. Re-registering is idempotent and
        // keeps accumulated priority; a diff for an unregistered entity would
        // otherwise be dropped.
        scheduler.register(persist_id.0, *cfg.uplink_hz.end());

        let seq_num = seq.next.entry(entity).or_insert(0);
        let tick = *seq_num;
        *seq_num += 1;

        scheduler.queue(DiffUplink {
            cell: cell.0,
            grid: GridId::ROOT,
            entity: persist_id.0,
            tick: Tick::new(tick),
            kind: RecordKind::ComponentDiff,
            payload: diff.payload.clone(),
            seq: tick,
            lease_id: Some(lease_id),
            authority_seq: Some(authority.seq),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_app::prelude::*;
    use bevy_replicon::shared::replication::registry::FnsId;
    use orrery_protocol::CellId;

    fn app() -> App {
        let mut app = App::new();
        app.init_resource::<UplinkSeq>()
            .init_resource::<UplinkScheduler>()
            .init_resource::<PersistClientConfig>()
            .add_message::<ComponentDiff>()
            .add_systems(Update, feed_uplink);
        app
    }

    #[test]
    fn locally_authoritative_entities_are_uplinked() {
        let mut app = app();
        let entity = app
            .world_mut()
            .spawn((
                PersistId::new(1),
                Cell(CellId::ROOT),
                LocallyAuthoritative,
                Authority {
                    holder: None,
                    seq: Default::default(),
                },
                AuthorityPhase::LocalGranted {
                    lease_id: orrery_protocol::LeaseId(1),
                    expires_at_ms: 10_000,
                },
            ))
            .id();

        app.world_mut()
            .resource_mut::<Messages<ComponentDiff>>()
            .write(ComponentDiff {
                entity,
                fns_id: FnsId::new(0),
                payload: bytes::Bytes::from_static(b"hp=50"),
            });

        app.update();

        let scheduler = app.world().resource::<UplinkScheduler>();
        assert!(scheduler.has_pending(orrery_protocol::PersistId::new(1)));
        let seq = app.world().resource::<UplinkSeq>();
        assert_eq!(seq.next.get(&entity), Some(&1));
    }

    #[test]
    fn non_authoritative_entities_are_ignored() {
        let mut app = app();
        let entity = app
            .world_mut()
            .spawn((PersistId::new(2), Cell(CellId::ROOT)))
            .id();

        app.world_mut()
            .resource_mut::<Messages<ComponentDiff>>()
            .write(ComponentDiff {
                entity,
                fns_id: FnsId::new(0),
                payload: bytes::Bytes::from_static(b"hp=50"),
            });

        app.update();

        let scheduler = app.world().resource::<UplinkScheduler>();
        assert!(!scheduler.has_pending(orrery_protocol::PersistId::new(2)));
    }
}
