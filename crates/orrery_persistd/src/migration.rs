//! Composition-time component migration and lazy checkpoint application.
//!
//! This is deliberately separate from [`orrery_core::Ruleset`]. A game builds
//! a [`MigrationRegistry`] beside its adjudication registrations, then wraps
//! its checkpoint store in [`MigratingStore`]. The frozen checkpoint traits do
//! not change, and a game with no migration era need not install the wrapper.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use orrery_core::{ComponentMigrator, ComponentTypeId};
use orrery_protocol::atrest::SchemaVersion;
use orrery_protocol::{CellId, GridId};

use crate::actor::{EntityRecord, SnapshotPage};
use crate::checkpoint::{CheckpointData, CheckpointError, CheckpointStore, ColdCellReader};
use crate::schema::ComponentBag;

/// Composition-time registry of current component schemas and adjacent steps.
///
/// Components must be declared even when their current version is zero. That
/// makes an accidentally empty registry fail closed instead of interpreting a
/// stale payload as current. Steps are keyed exactly by `(component,
/// from_version)` and always advance one version.
#[derive(Clone, Default)]
pub struct MigrationRegistry {
    current: BTreeMap<ComponentTypeId, SchemaVersion>,
    steps: BTreeMap<(ComponentTypeId, SchemaVersion), ComponentMigrator>,
}

impl core::fmt::Debug for MigrationRegistry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MigrationRegistry")
            .field("current", &self.current)
            .field("step_count", &self.steps.len())
            .finish()
    }
}

impl MigrationRegistry {
    /// Construct an empty registry. An installed empty registry refuses every
    /// non-empty framed bag; omitting [`MigratingStore`] disables migration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare the schema version this binary reads and writes for a component.
    ///
    /// Re-declaring a component replaces its target, which keeps composition
    /// reloads idempotent in the same way as `AdjudicationExecutor::register`.
    pub fn declare(&mut self, component: ComponentTypeId, current: SchemaVersion) {
        self.current.insert(component, current);
    }

    /// Register one pure adjacent migration step, `from_version ->
    /// from_version + 1`.
    ///
    /// Re-registering the same key replaces the function. The target schema is
    /// declared separately so removing this call leaves a detectable gap.
    pub fn register(
        &mut self,
        component: ComponentTypeId,
        from_version: SchemaVersion,
        migrator: ComponentMigrator,
    ) {
        self.steps.insert((component, from_version), migrator);
    }

    /// Migrate every slot in a framed bag to its declared current version.
    ///
    /// # Errors
    ///
    /// Refuses undeclared components, future versions, a missing adjacent
    /// step, a failed step, or a bag whose encoded bytes are malformed.
    pub fn migrate_bag(&self, encoded: &[u8]) -> Result<ComponentBag, MigrationError> {
        let mut bag = ComponentBag::decode(encoded).map_err(MigrationError::Decode)?;
        for slot in &mut bag.slots {
            let Some(&current) = self.current.get(&slot.component) else {
                return Err(MigrationError::UnregisteredComponent {
                    component: slot.component,
                    found: slot.schema_version,
                });
            };
            if slot.schema_version > current {
                return Err(MigrationError::FutureVersion {
                    component: slot.component,
                    found: slot.schema_version,
                    current,
                });
            }
            while slot.schema_version < current {
                let from = slot.schema_version;
                let Some(step) = self.steps.get(&(slot.component, from)) else {
                    return Err(MigrationError::MissingStep {
                        component: slot.component,
                        from,
                        current,
                    });
                };
                slot.payload = step(slot.payload.clone(), from).map_err(|message| {
                    MigrationError::StepFailed {
                        component: slot.component,
                        from,
                        message,
                    }
                })?;
                slot.schema_version =
                    from.checked_add(1).ok_or(MigrationError::VersionOverflow {
                        component: slot.component,
                    })?;
            }
        }
        Ok(bag)
    }

    fn migrate_record(&self, record: &mut EntityRecord) -> Result<bool, MigrationError> {
        let before = ComponentBag::decode(&record.components).map_err(MigrationError::Decode)?;
        let encoded_floor = before.schema_floor();
        if encoded_floor != record.schema_floor {
            return Err(MigrationError::FloorMismatch {
                envelope: record.schema_floor,
                bag: encoded_floor,
            });
        }
        let after = self.migrate_bag(&record.components)?;
        if after == before {
            return Ok(false);
        }
        record.components = after.encode().map_err(MigrationError::Encode)?;
        record.schema_floor = after.schema_floor();
        record.dirty = true;
        Ok(true)
    }

    #[cfg(any(test, feature = "fdb"))]
    pub(crate) fn sweep_floor(&self) -> Option<SchemaVersion> {
        self.current.values().copied().max()
    }
}

#[cfg(any(test, feature = "fdb"))]
pub(crate) fn migrate_world_value(
    registry: &MigrationRegistry,
    value: &[u8],
) -> Result<Option<Vec<u8>>, MigrationError> {
    let Some(sweep_floor) = registry.sweep_floor() else {
        return Ok(None);
    };
    if !crate::keyspace::world_value_is_stale(value, sweep_floor) {
        return Ok(None);
    }
    let components = crate::keyspace::world_value_components(value)
        .ok_or_else(|| MigrationError::MalformedWorldValue(value.first().copied()))?;
    let schema_floor = crate::keyspace::world_value_schema_floor(value)
        .ok_or_else(|| MigrationError::MalformedWorldValue(value.first().copied()))?;
    let mut record = EntityRecord {
        components: bytes::Bytes::copy_from_slice(components),
        dirty: false,
        schema_floor,
    };
    if !registry.migrate_record(&mut record)? {
        return Ok(None);
    }
    Ok(Some(crate::keyspace::encode_versioned_live_value(
        record.schema_floor,
        &record.components,
    )))
}

/// A migration was unsafe or impossible to apply.
#[derive(Debug)]
pub enum MigrationError {
    /// The component bag was not valid W1 framing. Legacy opaque v0 bags need
    /// an explicit out-of-band bootstrap before per-component migration.
    Decode(postcard::Error),
    /// Encoding a migrated bag failed.
    Encode(postcard::Error),
    /// A row advertised a live envelope but lacked its required fields.
    MalformedWorldValue(Option<u8>),
    /// No current schema was declared for a component.
    UnregisteredComponent {
        /// Component in the stale slot.
        component: ComponentTypeId,
        /// Version found in the slot.
        found: SchemaVersion,
    },
    /// The stored value comes from a newer schema than this binary knows.
    FutureVersion {
        /// Component in the future slot.
        component: ComponentTypeId,
        /// Version found in the slot.
        found: SchemaVersion,
        /// Version understood by this binary.
        current: SchemaVersion,
    },
    /// The gapless adjacent chain is missing a required step.
    MissingStep {
        /// Component that could not advance.
        component: ComponentTypeId,
        /// Missing step's source version.
        from: SchemaVersion,
        /// Declared target version.
        current: SchemaVersion,
    },
    /// A registered migration rejected its payload.
    StepFailed {
        /// Component whose step failed.
        component: ComponentTypeId,
        /// Failing source version.
        from: SchemaVersion,
        /// Stable diagnostic supplied by the migrator.
        message: &'static str,
    },
    /// Incrementing a schema version overflowed.
    VersionOverflow {
        /// Component at `u32::MAX`.
        component: ComponentTypeId,
    },
    /// W1's envelope marker disagreed with the framed bag it summarizes.
    FloorMismatch {
        /// Floor stored in the `world/` envelope or entity record.
        envelope: SchemaVersion,
        /// Floor derived from the component slots.
        bag: SchemaVersion,
    },
}

impl core::fmt::Display for MigrationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Decode(error) => write!(f, "component bag decode: {error}"),
            Self::Encode(error) => write!(f, "component bag encode: {error}"),
            Self::MalformedWorldValue(tag) => write!(f, "malformed world value with tag {tag:?}"),
            Self::UnregisteredComponent { component, found } => write!(
                f,
                "component {} at schema {found} has no declaration",
                component.0
            ),
            Self::FutureVersion {
                component,
                found,
                current,
            } => write!(
                f,
                "component {} schema {found} is newer than registered {current}",
                component.0
            ),
            Self::MissingStep {
                component,
                from,
                current,
            } => write!(
                f,
                "component {} has no migration from {from} toward {current}",
                component.0
            ),
            Self::StepFailed {
                component,
                from,
                message,
            } => write!(
                f,
                "component {} migration from {from} failed: {message}",
                component.0
            ),
            Self::VersionOverflow { component } => {
                write!(f, "component {} schema version overflow", component.0)
            }
            Self::FloorMismatch { envelope, bag } => {
                write!(
                    f,
                    "world schema floor {envelope} disagrees with bag floor {bag}"
                )
            }
        }
    }
}

impl core::error::Error for MigrationError {}

/// Optional sweep controls. The default is disabled (D38 clause (a), W2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationSweepConfig {
    /// Whether a background sweep may be spawned.
    pub enabled: bool,
    /// Delay between complete passes over the configured ranges.
    pub interval: Duration,
    /// Maximum rows offered to the store in one range pass.
    pub rows_per_pass: usize,
}

impl Default for MigrationSweepConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: Duration::from_secs(60),
            rows_per_pass: 1_000,
        }
    }
}

/// Additive composition configuration for lazy migration and its optional
/// sweep.
#[derive(Debug, Clone, Default)]
pub struct MigrationConfig {
    /// Registered component targets and adjacent migration steps.
    pub registry: MigrationRegistry,
    /// Background sweep controls; disabled by default.
    pub sweep: MigrationSweepConfig,
}

/// A checkpoint/cold-reader adapter that applies registered migrations lazily.
pub struct MigratingStore<S> {
    inner: S,
    config: MigrationConfig,
}

impl<S> MigratingStore<S> {
    /// Compose migration around an existing durable adapter.
    #[must_use]
    pub const fn new(inner: S, config: MigrationConfig) -> Self {
        Self { inner, config }
    }

    /// Access the wrapped adapter.
    #[must_use]
    pub const fn inner(&self) -> &S {
        &self.inner
    }

    /// Access the composition-time migration configuration.
    #[must_use]
    pub const fn config(&self) -> &MigrationConfig {
        &self.config
    }

    fn migrate_page(&self, page: &mut SnapshotPage) -> Result<(), CheckpointError> {
        migrate_records(&self.config.registry, page.entities.values_mut())
    }
}

fn migrate_records<'a>(
    registry: &MigrationRegistry,
    records: impl Iterator<Item = &'a mut EntityRecord>,
) -> Result<(), CheckpointError> {
    for record in records {
        registry
            .migrate_record(record)
            .map_err(|error| CheckpointError::Store(format!("schema migration: {error}")))?;
    }
    Ok(())
}

#[async_trait::async_trait]
impl<S: CheckpointStore> CheckpointStore for MigratingStore<S> {
    async fn checkpoint(&self, data: &CheckpointData) -> Result<(), CheckpointError> {
        self.inner.checkpoint(data).await
    }

    async fn load(
        &self,
        shard: CellId,
        grid: GridId,
    ) -> Result<Option<CheckpointData>, CheckpointError> {
        let Some(mut data) = self.inner.load(shard, grid).await? else {
            return Ok(None);
        };
        migrate_records(&self.config.registry, data.entities.values_mut())?;
        Ok(Some(data))
    }

    async fn delete(&self, shard: CellId, grid: GridId) -> Result<(), CheckpointError> {
        self.inner.delete(shard, grid).await
    }
}

#[async_trait::async_trait]
impl<S: ColdCellReader> ColdCellReader for MigratingStore<S> {
    async fn read_cold(
        &self,
        grid: GridId,
        cell: CellId,
    ) -> Result<Option<SnapshotPage>, CheckpointError> {
        let Some(mut page) = self.inner.read_cold(grid, cell).await? else {
            return Ok(None);
        };
        self.migrate_page(&mut page)?;
        Ok(Some(page))
    }
}

/// Raw-store seam used by the optional background sweep.
#[async_trait::async_trait]
pub trait MigrationSweepTarget: Send + Sync + 'static {
    /// Migrate at most `limit` stale rows in one cell subtree.
    async fn sweep_migrations(
        &self,
        registry: &MigrationRegistry,
        grid: GridId,
        cell: CellId,
        limit: usize,
    ) -> Result<usize, CheckpointError>;
}

/// Handle for a running optional migration sweep.
pub struct MigrationSweeper {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl MigrationSweeper {
    /// Whether the config actually enabled a background task.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.task.is_some()
    }
}

impl Drop for MigrationSweeper {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Spawn the optional cold-range sweep.
///
/// A disabled config returns an inert handle without polling `target`. Lazy
/// checkpoint and area reads remain active independently in [`MigratingStore`].
#[must_use]
pub fn spawn_migration_sweep<S: MigrationSweepTarget>(
    target: Arc<S>,
    registry: MigrationRegistry,
    ranges: Vec<(GridId, CellId)>,
    config: MigrationSweepConfig,
) -> MigrationSweeper {
    if !config.enabled {
        return MigrationSweeper { task: None };
    }
    let task = tokio::spawn(async move {
        loop {
            for &(grid, cell) in &ranges {
                if let Err(error) = target
                    .sweep_migrations(&registry, grid, cell, config.rows_per_pass)
                    .await
                {
                    tracing::warn!(%grid, %cell, %error, "component migration sweep failed");
                }
            }
            tokio::time::sleep(config.interval).await;
        }
    });
    MigrationSweeper { task: Some(task) }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use bytes::Bytes;
    use orrery_protocol::{Epoch, Lsn, PersistId};

    use super::*;
    use crate::checkpoint::MemCheckpointStore;
    use crate::schema::ComponentSlot;

    const COMPONENT: ComponentTypeId = ComponentTypeId(17);
    const ENTITY: PersistId = PersistId::new(9);

    fn append_version(payload: Bytes, from: SchemaVersion) -> Result<Bytes, &'static str> {
        let mut bytes = payload.to_vec();
        bytes.push(u8::try_from(from + 1).map_err(|_| "test version overflow")?);
        Ok(Bytes::from(bytes))
    }

    fn bag(version: SchemaVersion, payload: &'static [u8]) -> Bytes {
        ComponentBag {
            slots: vec![ComponentSlot {
                component: COMPONENT,
                schema_version: version,
                payload: Bytes::from_static(payload),
            }],
        }
        .encode()
        .expect("test bag encodes")
    }

    fn checkpoint(version: SchemaVersion) -> CheckpointData {
        CheckpointData {
            shard: CellId::ROOT,
            grid: GridId::ROOT,
            node_id: 1,
            epoch: Epoch::new(1),
            watermark: Lsn::new(1, 1),
            entities: HashMap::from([(
                ENTITY,
                EntityRecord {
                    components: bag(version, b"old"),
                    dirty: false,
                    schema_floor: version,
                },
            )]),
            by_cell: HashMap::from([(ENTITY, CellId::ROOT)]),
            tombstones: HashMap::new(),
            superseded: Default::default(),
            taken_at_ms: 1,
        }
    }

    fn registry_with_step() -> MigrationRegistry {
        let mut registry = MigrationRegistry::new();
        registry.declare(COMPONENT, 1);
        registry.register(COMPONENT, 0, append_version);
        registry
    }

    #[tokio::test]
    async fn checkpoint_load_applies_registered_migration_lazily() {
        let inner = MemCheckpointStore::new();
        inner.checkpoint(&checkpoint(0)).await.expect("plant stale");
        let store = MigratingStore::new(
            inner,
            MigrationConfig {
                registry: registry_with_step(),
                sweep: MigrationSweepConfig::default(),
            },
        );

        let loaded = store
            .load(CellId::ROOT, GridId::ROOT)
            .await
            .expect("load")
            .expect("checkpoint");
        let record = &loaded.entities[&ENTITY];
        let migrated = ComponentBag::decode(&record.components).expect("migrated bag");
        assert_eq!(migrated.slots[0].schema_version, 1);
        assert_eq!(migrated.slots[0].payload.as_ref(), b"old\x01");
        assert_eq!(record.schema_floor, 1);
        assert!(record.dirty, "lazy migration must schedule write-back");
        assert!(!store.config().sweep.enabled, "the sweep stayed disabled");
    }

    #[tokio::test]
    async fn missing_registration_refuses_stale_checkpoint() {
        let inner = MemCheckpointStore::new();
        inner.checkpoint(&checkpoint(0)).await.expect("plant stale");
        let store = MigratingStore::new(inner, MigrationConfig::default());

        let error = store
            .load(CellId::ROOT, GridId::ROOT)
            .await
            .expect_err("an empty registry must fail closed");
        assert!(error.to_string().contains("has no declaration"), "{error}");
    }

    #[tokio::test]
    async fn missing_adjacent_step_refuses_stale_checkpoint() {
        let inner = MemCheckpointStore::new();
        inner.checkpoint(&checkpoint(0)).await.expect("plant stale");
        let mut registry = MigrationRegistry::new();
        registry.declare(COMPONENT, 1);
        let store = MigratingStore::new(
            inner,
            MigrationConfig {
                registry,
                sweep: MigrationSweepConfig::default(),
            },
        );

        let error = store
            .load(CellId::ROOT, GridId::ROOT)
            .await
            .expect_err("a missing adjacent registration must fail closed");
        assert!(error.to_string().contains("no migration from 0"), "{error}");
    }

    struct ColdPage(CheckpointData);

    #[async_trait::async_trait]
    impl ColdCellReader for ColdPage {
        async fn read_cold(
            &self,
            _grid: GridId,
            _cell: CellId,
        ) -> Result<Option<SnapshotPage>, CheckpointError> {
            Ok(Some(SnapshotPage {
                entities: self.0.entities.clone(),
            }))
        }
    }

    #[tokio::test]
    async fn area_read_applies_registered_migration_lazily() {
        let store = MigratingStore::new(
            ColdPage(checkpoint(0)),
            MigrationConfig {
                registry: registry_with_step(),
                sweep: MigrationSweepConfig::default(),
            },
        );
        let page = store
            .read_cold(GridId::ROOT, CellId::ROOT)
            .await
            .expect("area read")
            .expect("page");
        let record = &page.entities[&ENTITY];
        let migrated = ComponentBag::decode(&record.components).expect("migrated bag");
        assert_eq!(migrated.slots[0].schema_version, 1);
        assert_eq!(migrated.slots[0].payload.as_ref(), b"old\x01");
        assert!(!store.config().sweep.enabled, "the sweep stayed disabled");
    }

    #[test]
    fn sweep_rewrites_a_stale_world_value_through_the_same_registry() {
        let stale_bag = bag(0, b"old");
        let stale = crate::keyspace::encode_versioned_live_value(0, &stale_bag);
        let migrated = migrate_world_value(&registry_with_step(), &stale)
            .expect("migration succeeds")
            .expect("stale row rewrites");
        assert_eq!(
            crate::keyspace::world_value_schema_floor(&migrated),
            Some(1)
        );
        let migrated_bag = ComponentBag::decode(
            crate::keyspace::world_value_components(&migrated).expect("live world row"),
        )
        .expect("migrated bag");
        assert_eq!(migrated_bag.slots[0].payload.as_ref(), b"old\x01");
    }

    #[derive(Default)]
    struct CountingSweep(AtomicUsize);

    #[async_trait::async_trait]
    impl MigrationSweepTarget for CountingSweep {
        async fn sweep_migrations(
            &self,
            _registry: &MigrationRegistry,
            _grid: GridId,
            _cell: CellId,
            _limit: usize,
        ) -> Result<usize, CheckpointError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(0)
        }
    }

    #[tokio::test]
    async fn disabled_sweep_is_inert() {
        let target = Arc::new(CountingSweep::default());
        let sweep = spawn_migration_sweep(
            Arc::clone(&target),
            registry_with_step(),
            vec![(GridId::ROOT, CellId::ROOT)],
            MigrationSweepConfig::default(),
        );
        tokio::task::yield_now().await;
        assert!(!sweep.is_running());
        assert_eq!(target.0.load(Ordering::SeqCst), 0);
    }
}
