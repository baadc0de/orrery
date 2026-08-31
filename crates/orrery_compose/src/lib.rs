//! Link-time composition data for an Orrery ruleset.
//!
//! The composition root is deliberately a plain struct of tables. It describes
//! a build and is validated before registration; it neither changes nor
//! dispatches the existing [`orrery_core::Ruleset`] contract.

#![warn(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};

use orrery_core::{ComponentTypeId, CoreClass};
use orrery_protocol::{RulesetId, SchemaVersion};

pub mod registry;

/// A stable name for a game family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GameId(pub &'static str);

/// A manifest framing version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ManifestFormatVersion(pub u32);

/// A statically linked domain-module identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleId(pub &'static str);

/// A monotone version for one domain module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleVersion(pub u32);

/// A named section of a ruleset's `CoreState` owned by one module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StateSectionId(pub &'static str);

/// A named external-input vocabulary slice owned by one module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InputVocabularyId(pub &'static str);

/// A named domain-event vocabulary slice owned by one module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventVocabularyId(pub &'static str);

/// A named canonical schedule stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScheduleStageId(pub &'static str);

/// A named canonical system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SystemId(pub &'static str);

/// An identifier for the single determinism profile this build claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProfileId(pub &'static str);

/// The witness projection framing version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectionVersion(pub u32);

/// A component schema key, deliberately named rather than a bare tuple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ComponentSchemaId {
    /// The permanent game-assigned component identifier.
    pub component: ComponentTypeId,
    /// The current schema version for that component.
    pub version: SchemaVersion,
}

/// Persistence capability for one component schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistenceCapability {
    /// The component is not persisted.
    None,
    /// The component is bulk-persisted.
    Bulk,
    /// The component is persisted through critical intent transactions.
    Critical,
}

/// Rollback capability for one component schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollbackCapability {
    /// The component is excluded from rollback.
    Excluded,
    /// The component is included in rollback.
    Included,
}

/// Witnessing capability for one component schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WitnessCapability {
    /// The component is not watched.
    Unwatched,
    /// The component is invariant-checked.
    InvariantChecked,
    /// The component is replay-adjudicated.
    ReplayAdjudicated,
}

/// Replication capability for one component schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicationCapability {
    /// The component is not replicated.
    None,
    /// The component is interest-replicated.
    InterestReplicated,
}

/// Write-authority capability for one component schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteAuthorityCapability {
    /// The local process owns writes.
    Local,
    /// A lease holder owns writes.
    LeaseHolder,
    /// The island's weak authority owns writes.
    IslandWeak,
    /// A cluster transaction owns writes.
    ClusterTransaction,
}

/// The five independent component capabilities required by D45.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentCapabilities {
    /// Persistence policy.
    pub persistence: PersistenceCapability,
    /// Rollback membership.
    pub rollback: RollbackCapability,
    /// Witnessing policy.
    pub witness: WitnessCapability,
    /// Replication policy.
    pub replication: ReplicationCapability,
    /// Write authority policy.
    pub write_authority: WriteAuthorityCapability,
}

impl ComponentCapabilities {
    /// Whether the declaration says this component reaches durable storage.
    ///
    /// The one question D-3 persistence asks of a declaration, and the reason
    /// the differential harness no longer needs a trait method: `P0` writes
    /// no at-rest slot, `P1` and `P2` each write one. An **undeclared**
    /// component has no capabilities at all and so writes nothing either —
    /// D45 clause (c)'s "no declaration, no capability", which is the same
    /// fail-closed answer the retired `classify_component` default gave.
    #[must_use]
    pub const fn is_persisted(self) -> bool {
        !matches!(self.persistence, PersistenceCapability::None)
    }
}

/// One of D45 clause (d)'s named capability profiles.
///
/// **Derived vocabulary, never an authored datum** (ADR-0045 clause (g), A5
/// §6.2). A profile is *computed* from the five declared dimensions by
/// [`profile_of`]; nothing in the tree authors, persists, hashes or routes on
/// it. It exists so a reviewer and a refusal message can keep speaking the
/// names the documentation set speaks, and so a validator has a cheap
/// tripwire for a typo'd policy: a declaration matching no profile is
/// [`None`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityProfile {
    /// `P1`/`P2` · R per R6 · `W2` · `N1` · `A1` — verifiable core state.
    Core,
    /// `P1` · `R0` · `W1` · `N1` · `A1` — persisted, invariant-checked only.
    Bulk,
    /// All zeros — never persisted, never verified, never replicated.
    CosmeticLocal,
    /// `P0` · `R0` · `W0` · `N1` · `A2` — replicated in-island, never stored.
    EphemeralShared,
    /// `P2` · `R0` · `W0` · `N0` · `A3` — transaction-final ledger rows.
    CriticalLedger,
}

impl CapabilityProfile {
    /// This profile's [`CoreClass`] name, where the three-valued enum has one.
    ///
    /// [`None`] for [`CapabilityProfile::EphemeralShared`] and
    /// [`CapabilityProfile::CriticalLedger`], which is not an omission: those
    /// are the two rows ADR-0045 clause (d) names as the demonstration that
    /// `CoreClass` "files an ephemeral projectile and a local UI component
    /// under the same value, and gives a ledger row no value at all".
    #[must_use]
    pub const fn core_class(self) -> Option<CoreClass> {
        match self {
            CapabilityProfile::Core => Some(CoreClass::Core),
            CapabilityProfile::Bulk => Some(CoreClass::Bulk),
            CapabilityProfile::CosmeticLocal => Some(CoreClass::Cosmetic),
            CapabilityProfile::EphemeralShared | CapabilityProfile::CriticalLedger => None,
        }
    }
}

/// The profile a declaration's five dimensions name, or [`None`] for a
/// combination ADR-0045 clause (d) does not name.
#[must_use]
pub const fn profile_of(capabilities: ComponentCapabilities) -> Option<CapabilityProfile> {
    use PersistenceCapability as P;
    use ReplicationCapability as N;
    use RollbackCapability as R;
    use WitnessCapability as W;
    use WriteAuthorityCapability as A;

    match (
        capabilities.persistence,
        capabilities.rollback,
        capabilities.witness,
        capabilities.replication,
        capabilities.write_authority,
    ) {
        // Rollback membership is R6's to decide for core state, so the Core
        // profile is deliberately free in that dimension.
        (P::Bulk | P::Critical, _, W::ReplayAdjudicated, N::InterestReplicated, A::LeaseHolder) => {
            Some(CapabilityProfile::Core)
        }
        (P::Bulk, R::Excluded, W::InvariantChecked, N::InterestReplicated, A::LeaseHolder) => {
            Some(CapabilityProfile::Bulk)
        }
        (P::None, R::Excluded, W::Unwatched, N::None, A::Local) => {
            Some(CapabilityProfile::CosmeticLocal)
        }
        (P::None, R::Excluded, W::Unwatched, N::InterestReplicated, A::IslandWeak) => {
            Some(CapabilityProfile::EphemeralShared)
        }
        (P::Critical, R::Excluded, W::Unwatched, N::None, A::ClusterTransaction) => {
            Some(CapabilityProfile::CriticalLedger)
        }
        _ => None,
    }
}

/// One component-schema declaration in the manifest's schema table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentSchemaManifest {
    /// The module which owns this component's state section.
    pub owner: ModuleId,
    /// The stable schema identifier and current schema version.
    pub id: ComponentSchemaId,
    /// The component's five independent capabilities.
    pub capabilities: ComponentCapabilities,
}

/// One named, statically linked domain module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleManifest {
    /// The module's stable identifier.
    pub id: ModuleId,
    /// The module's monotone version.
    pub version: ModuleVersion,
    /// Other declared modules this module requires.
    pub dependencies: &'static [ModuleId],
    /// The `CoreState` sections this module owns.
    pub state_sections: &'static [StateSectionId],
    /// The external input vocabulary slices this module owns.
    pub inputs: &'static [InputVocabularyId],
    /// The domain-event vocabulary slices this module owns.
    pub events: &'static [EventVocabularyId],
    /// Canonical schedule stages in which this module declares systems.
    pub schedule_stages: &'static [ScheduleStageId],
}

/// The policy used when a scheduler discovers ambiguous system ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmbiguityDetection {
    /// Reject an ambiguous canonical schedule before it can run.
    Error,
    /// Report an ambiguity without refusing composition.
    Warning,
}

/// The canonical executor's ordering policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorPolicy {
    /// Run every system in the table's declared order on one thread.
    SingleThreaded,
}

/// The ordered systems in one canonical stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleStageManifest {
    /// The stable stage name.
    pub id: ScheduleStageId,
    /// Systems in their canonical execution order.
    pub systems: &'static [SystemId],
}

/// One declared canonical ordering edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleOrderingEdge {
    /// The system which must run first.
    pub before: SystemId,
    /// The system which must run afterwards.
    pub after: SystemId,
}

/// One ambiguity reported by schedule construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleAmbiguity {
    /// One conflicting system.
    pub first: SystemId,
    /// The other conflicting system.
    pub second: SystemId,
}

/// The schedule-topology table which D43 places in the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanonicalSchedule {
    /// Canonically ordered stages and their systems.
    pub stages: &'static [ScheduleStageManifest],
    /// Declared ordering constraints.
    pub ordering_edges: &'static [ScheduleOrderingEdge],
    /// Ambiguities reported while constructing the schedule.
    pub ambiguities: &'static [ScheduleAmbiguity],
    /// The required error-level ambiguity policy.
    pub ambiguity_detection: AmbiguityDetection,
    /// The executor policy included in the schedule digest.
    pub executor_policy: ExecutorPolicy,
}

/// A canonical constant carried in the registry beside component schemas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanonicalConstant {
    /// The stable constant name.
    pub name: &'static str,
    /// The declared value.
    pub value: u64,
}

/// A deliberately retired component schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemovedComponentSchema {
    /// The retired schema identity.
    pub id: ComponentSchemaId,
}

/// A game build's link-time composition manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompatibilityManifest {
    /// Stable game-family name.
    pub game_id: GameId,
    /// Version of the manifest framing.
    pub manifest_format_version: ManifestFormatVersion,
    /// Exact wire protocol version.
    pub protocol_version: u16,
    /// Advisory toolchain stamp, not an admission axis.
    pub toolchain_stamp: &'static str,
    /// The build identity used by evidence paths.
    pub ruleset: RulesetId,
    /// Statically linked domain-module table.
    pub modules: &'static [ModuleManifest],
    /// Component-schema and capability table.
    pub component_schemas: &'static [ComponentSchemaManifest],
    /// Canonical schedule topology table.
    pub schedule: CanonicalSchedule,
    /// Registry-owned canonical constants.
    pub canonical_constants: &'static [CanonicalConstant],
    /// Current witness projection framing version.
    pub projection_version: ProjectionVersion,
    /// The build's determinism-envelope identifier.
    pub profile_id: ProfileId,
    /// Explicitly retired component schemas.
    pub removed_components: &'static [RemovedComponentSchema],
}

impl CompatibilityManifest {
    /// The schema row this build declares for `component`, if it declares one.
    ///
    /// The single source every classification consumer reads. An undeclared
    /// component returns [`None`] and therefore has no capabilities — D45
    /// clause (c)'s fail-closed zeros — rather than falling back to a second
    /// statement of the same fact somewhere else.
    #[must_use]
    pub fn declaration(&self, component: ComponentTypeId) -> Option<ComponentSchemaManifest> {
        self.component_schemas
            .iter()
            .copied()
            .find(|schema| schema.id.component == component)
    }
}

/// A composition-time refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositionError {
    /// A module identifier appears more than once.
    DuplicateModuleId(ModuleId),
    /// A module declares a dependency that no module table row provides.
    MissingDependency {
        /// The module making the missing declaration.
        module: ModuleId,
        /// The undeclared dependency.
        dependency: ModuleId,
    },
    /// The declared dependency graph contains a cycle.
    CyclicDependency {
        /// A module on the detected cycle.
        module: ModuleId,
    },
    /// A permanent component identifier appears in more than one schema row.
    DuplicateComponentTypeId(ComponentTypeId),
    /// A schema is owned by a module absent from the module table.
    MissingSchemaOwner(ModuleId),
    /// A module declares systems in a stage absent from the canonical schedule.
    UndeclaredScheduleStage {
        /// The module making the declaration.
        module: ModuleId,
        /// The stage absent from the canonical schedule.
        stage: ScheduleStageId,
    },
    /// A canonical schedule is configured to tolerate ambiguity.
    CanonicalScheduleDoesNotRejectAmbiguity,
    /// A canonical schedule has a reported ambiguity.
    CanonicalScheduleAmbiguity(ScheduleAmbiguity),
}

/// Compute D43 clause (g)'s schedule digest.
///
/// blake3 over a canonical serialization of: the ordered stage list; each
/// stage's ordered system names; every declared ordering edge, sorted
/// lexicographically; the ambiguity-detection setting; and the executor
/// policy. It exists to catch scheduler-topology drift that state goldens
/// cannot see — goldens hash states, not graphs.
///
/// **Every field is length-prefixed** rather than separator-delimited. A
/// separator makes `["a", "b"]` and `["a|b"]` hash alike, which would let a
/// stage rename disguise a system reorder; a digest that can be spoofed by
/// choosing a name is worse than no digest, because it is trusted.
///
/// Edges are sorted here rather than assumed sorted at the declaration site:
/// the clause says the digest covers the *set* of edges, so two manifests
/// declaring the same constraints in a different textual order must agree.
#[must_use]
pub fn schedule_digest(schedule: &CanonicalSchedule) -> [u8; 32] {
    fn push_str(out: &mut Vec<u8>, value: &str) {
        let len = u32::try_from(value.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(value.as_bytes());
    }
    fn push_len(out: &mut Vec<u8>, value: usize) {
        out.extend_from_slice(&u32::try_from(value).unwrap_or(u32::MAX).to_le_bytes());
    }

    let mut bytes = Vec::new();
    push_len(&mut bytes, schedule.stages.len());
    for stage in schedule.stages {
        push_str(&mut bytes, stage.id.0);
        push_len(&mut bytes, stage.systems.len());
        for system in stage.systems {
            push_str(&mut bytes, system.0);
        }
    }
    let mut edges = schedule
        .ordering_edges
        .iter()
        .map(|edge| (edge.before.0, edge.after.0))
        .collect::<Vec<_>>();
    edges.sort_unstable();
    push_len(&mut bytes, edges.len());
    for (before, after) in edges {
        push_str(&mut bytes, before);
        push_str(&mut bytes, after);
    }
    bytes.push(match schedule.ambiguity_detection {
        AmbiguityDetection::Error => 0,
        AmbiguityDetection::Warning => 1,
    });
    bytes.push(match schedule.executor_policy {
        ExecutorPolicy::SingleThreaded => 0,
    });
    *blake3::hash(&bytes).as_bytes()
}

/// Validate a link-time composition manifest before registration.
///
/// Dependency cycles are detected over the complete transitive closure of the
/// declared dependency graph. There is no runtime module loading or dispatch.
pub fn validate(manifest: &CompatibilityManifest) -> Result<(), CompositionError> {
    let module_ids = validate_module_ids(manifest.modules)?;
    validate_dependencies(manifest.modules, &module_ids)?;
    validate_component_schemas(manifest.component_schemas, &module_ids)?;
    validate_schedule(manifest.modules, &manifest.schedule)
}

fn validate_module_ids(modules: &[ModuleManifest]) -> Result<BTreeSet<ModuleId>, CompositionError> {
    let mut module_ids = BTreeSet::new();
    for module in modules {
        if !module_ids.insert(module.id) {
            return Err(CompositionError::DuplicateModuleId(module.id));
        }
    }
    Ok(module_ids)
}

fn validate_dependencies(
    modules: &[ModuleManifest],
    module_ids: &BTreeSet<ModuleId>,
) -> Result<(), CompositionError> {
    for module in modules {
        for dependency in module.dependencies {
            if !module_ids.contains(dependency) {
                return Err(CompositionError::MissingDependency {
                    module: module.id,
                    dependency: *dependency,
                });
            }
        }
    }

    let modules_by_id = modules_by_id(modules);
    let mut visits = BTreeMap::new();
    for module in modules {
        visit_module(module.id, &modules_by_id, &mut visits)?;
    }
    Ok(())
}

fn modules_by_id(modules: &[ModuleManifest]) -> BTreeMap<ModuleId, &ModuleManifest> {
    let mut modules_by_id = BTreeMap::new();
    for module in modules {
        modules_by_id.insert(module.id, module);
    }
    modules_by_id
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visit {
    Visiting,
    Done,
}

fn visit_module(
    module_id: ModuleId,
    modules_by_id: &BTreeMap<ModuleId, &ModuleManifest>,
    visits: &mut BTreeMap<ModuleId, Visit>,
) -> Result<Visit, CompositionError> {
    match visits.get(&module_id) {
        Some(Visit::Visiting) => {
            return Err(CompositionError::CyclicDependency { module: module_id });
        }
        Some(Visit::Done) => return Ok(Visit::Done),
        None => {}
    }

    visits.insert(module_id, Visit::Visiting);
    let module = modules_by_id
        .get(&module_id)
        .expect("dependencies are validated before cycle detection");
    for dependency in module.dependencies {
        visit_module(*dependency, modules_by_id, visits)?;
    }
    visits.insert(module_id, Visit::Done);
    Ok(Visit::Done)
}

fn validate_component_schemas(
    schemas: &[ComponentSchemaManifest],
    module_ids: &BTreeSet<ModuleId>,
) -> Result<(), CompositionError> {
    let mut component_ids = BTreeSet::new();
    for schema in schemas {
        if !module_ids.contains(&schema.owner) {
            return Err(CompositionError::MissingSchemaOwner(schema.owner));
        }
        if !component_ids.insert(schema.id.component) {
            return Err(CompositionError::DuplicateComponentTypeId(
                schema.id.component,
            ));
        }
    }
    Ok(())
}

fn validate_schedule(
    modules: &[ModuleManifest],
    schedule: &CanonicalSchedule,
) -> Result<(), CompositionError> {
    let declared_stages = schedule
        .stages
        .iter()
        .map(|stage| stage.id)
        .collect::<BTreeSet<_>>();
    for module in modules {
        for stage in module.schedule_stages {
            if !declared_stages.contains(stage) {
                return Err(CompositionError::UndeclaredScheduleStage {
                    module: module.id,
                    stage: *stage,
                });
            }
        }
    }
    if schedule.ambiguity_detection != AmbiguityDetection::Error {
        return Err(CompositionError::CanonicalScheduleDoesNotRejectAmbiguity);
    }
    if let Some(ambiguity) = schedule.ambiguities.first() {
        return Err(CompositionError::CanonicalScheduleAmbiguity(*ambiguity));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::regolith;

    const MODULE_A: ModuleId = ModuleId("module-a");
    const MODULE_B: ModuleId = ModuleId("module-b");
    const MISSING_MODULE: ModuleId = ModuleId("missing-module");
    const MODULE_A_DEPENDS_ON_B: &[ModuleId] = &[MODULE_B];
    const MODULE_B_DEPENDS_ON_A: &[ModuleId] = &[MODULE_A];
    const MODULE_A_DEPENDS_ON_MISSING: &[ModuleId] = &[MISSING_MODULE];
    const EMPTY_MODULE_IDS: &[ModuleId] = &[];
    const EMPTY_STATE_SECTIONS: &[StateSectionId] = &[];
    const EMPTY_INPUTS: &[InputVocabularyId] = &[];
    const EMPTY_EVENTS: &[EventVocabularyId] = &[];
    const EMPTY_SCHEDULE_STAGES: &[ScheduleStageId] = &[];
    const EMPTY_COMPONENTS: &[ComponentSchemaManifest] = &[];
    const EMPTY_CONSTANTS: &[CanonicalConstant] = &[];
    const EMPTY_REMOVED_COMPONENTS: &[RemovedComponentSchema] = &[];
    const EMPTY_STAGES: &[ScheduleStageManifest] = &[];
    const EMPTY_EDGES: &[ScheduleOrderingEdge] = &[];
    const EMPTY_AMBIGUITIES: &[ScheduleAmbiguity] = &[];
    const AMBIGUITIES: &[ScheduleAmbiguity] = &[ScheduleAmbiguity {
        first: SystemId("first"),
        second: SystemId("second"),
    }];

    const MODULE_A_MANIFEST: ModuleManifest = ModuleManifest {
        id: MODULE_A,
        version: ModuleVersion(1),
        dependencies: EMPTY_MODULE_IDS,
        state_sections: EMPTY_STATE_SECTIONS,
        inputs: EMPTY_INPUTS,
        events: EMPTY_EVENTS,
        schedule_stages: EMPTY_SCHEDULE_STAGES,
    };
    const MODULE_B_MANIFEST: ModuleManifest = ModuleManifest {
        id: MODULE_B,
        version: ModuleVersion(1),
        dependencies: EMPTY_MODULE_IDS,
        state_sections: EMPTY_STATE_SECTIONS,
        inputs: EMPTY_INPUTS,
        events: EMPTY_EVENTS,
        schedule_stages: EMPTY_SCHEDULE_STAGES,
    };

    const fn manifest(
        modules: &'static [ModuleManifest],
        component_schemas: &'static [ComponentSchemaManifest],
        ambiguities: &'static [ScheduleAmbiguity],
    ) -> CompatibilityManifest {
        CompatibilityManifest {
            game_id: GameId("test"),
            manifest_format_version: ManifestFormatVersion(1),
            protocol_version: 6,
            toolchain_stamp: "test",
            ruleset: RulesetId {
                version: 1,
                digest: [0; 32],
            },
            modules,
            component_schemas,
            schedule: CanonicalSchedule {
                stages: EMPTY_STAGES,
                ordering_edges: EMPTY_EDGES,
                ambiguities,
                ambiguity_detection: AmbiguityDetection::Error,
                executor_policy: ExecutorPolicy::SingleThreaded,
            },
            canonical_constants: EMPTY_CONSTANTS,
            projection_version: ProjectionVersion(1),
            profile_id: ProfileId("d9"),
            removed_components: EMPTY_REMOVED_COMPONENTS,
        }
    }

    #[test]
    fn missing_dependency_refuses_composition() {
        const MODULES: &[ModuleManifest] = &[ModuleManifest {
            dependencies: MODULE_A_DEPENDS_ON_MISSING,
            ..MODULE_A_MANIFEST
        }];
        let error = validate(&manifest(MODULES, EMPTY_COMPONENTS, EMPTY_AMBIGUITIES));
        assert_eq!(
            error,
            Err(CompositionError::MissingDependency {
                module: MODULE_A,
                dependency: MISSING_MODULE,
            })
        );
    }

    #[test]
    fn cyclic_dependency_refuses_composition() {
        const MODULES: &[ModuleManifest] = &[
            ModuleManifest {
                dependencies: MODULE_A_DEPENDS_ON_B,
                ..MODULE_A_MANIFEST
            },
            ModuleManifest {
                dependencies: MODULE_B_DEPENDS_ON_A,
                ..MODULE_B_MANIFEST
            },
        ];
        let error = validate(&manifest(MODULES, EMPTY_COMPONENTS, EMPTY_AMBIGUITIES));
        assert_eq!(
            error,
            Err(CompositionError::CyclicDependency { module: MODULE_A })
        );
    }

    #[test]
    fn duplicate_schema_id_refuses_composition() {
        const MODULES: &[ModuleManifest] = &[MODULE_A_MANIFEST, MODULE_B_MANIFEST];
        const COMPONENT_SCHEMAS: &[ComponentSchemaManifest] = &[
            ComponentSchemaManifest {
                owner: MODULE_A,
                id: ComponentSchemaId {
                    component: regolith::COMPONENT_TYPE_IDS[0].id,
                    version: 1,
                },
                capabilities: ComponentCapabilities {
                    persistence: PersistenceCapability::Bulk,
                    rollback: RollbackCapability::Excluded,
                    witness: WitnessCapability::InvariantChecked,
                    replication: ReplicationCapability::InterestReplicated,
                    write_authority: WriteAuthorityCapability::LeaseHolder,
                },
            },
            ComponentSchemaManifest {
                owner: MODULE_B,
                id: ComponentSchemaId {
                    component: regolith::COMPONENT_TYPE_IDS[0].id,
                    version: 2,
                },
                capabilities: ComponentCapabilities {
                    persistence: PersistenceCapability::Bulk,
                    rollback: RollbackCapability::Excluded,
                    witness: WitnessCapability::InvariantChecked,
                    replication: ReplicationCapability::InterestReplicated,
                    write_authority: WriteAuthorityCapability::LeaseHolder,
                },
            },
        ];
        let error = validate(&manifest(MODULES, COMPONENT_SCHEMAS, EMPTY_AMBIGUITIES));
        assert_eq!(
            error,
            Err(CompositionError::DuplicateComponentTypeId(regolith::STATE))
        );
    }

    /// Length prefixes, not separators: two different topologies whose
    /// concatenated names coincide must not hash alike.
    ///
    /// Without this the digest is spoofable by choosing a name, and a
    /// spoofable digest is worse than none because it is trusted.
    #[test]
    fn the_digest_separates_topologies_whose_names_concatenate_alike() {
        const SPLIT: &[ScheduleStageManifest] = &[ScheduleStageManifest {
            id: ScheduleStageId("stage"),
            systems: &[SystemId("ab"), SystemId("c")],
        }];
        const JOINED: &[ScheduleStageManifest] = &[ScheduleStageManifest {
            id: ScheduleStageId("stage"),
            systems: &[SystemId("a"), SystemId("bc")],
        }];
        let base = CanonicalSchedule {
            stages: SPLIT,
            ordering_edges: EMPTY_EDGES,
            ambiguities: EMPTY_AMBIGUITIES,
            ambiguity_detection: AmbiguityDetection::Error,
            executor_policy: ExecutorPolicy::SingleThreaded,
        };
        let mut other = base;
        other.stages = JOINED;
        assert_ne!(schedule_digest(&base), schedule_digest(&other));
    }

    /// The empty schedule every game declared before it had systems hashes to
    /// one value, so the digest asserted nothing about any of them.
    #[test]
    fn the_empty_schedule_digest_is_the_same_for_every_game() {
        let empty = CanonicalSchedule {
            stages: EMPTY_STAGES,
            ordering_edges: EMPTY_EDGES,
            ambiguities: EMPTY_AMBIGUITIES,
            ambiguity_detection: AmbiguityDetection::Error,
            executor_policy: ExecutorPolicy::SingleThreaded,
        };
        assert_eq!(schedule_digest(&empty), schedule_digest(&empty));
        let mut populated = empty;
        const ONE: &[ScheduleStageManifest] = &[ScheduleStageManifest {
            id: ScheduleStageId("stage"),
            systems: &[SystemId("only")],
        }];
        populated.stages = ONE;
        assert_ne!(schedule_digest(&empty), schedule_digest(&populated));
    }

    const CORE_CAPABILITIES: ComponentCapabilities = ComponentCapabilities {
        persistence: PersistenceCapability::Bulk,
        rollback: RollbackCapability::Included,
        witness: WitnessCapability::ReplayAdjudicated,
        replication: ReplicationCapability::InterestReplicated,
        write_authority: WriteAuthorityCapability::LeaseHolder,
    };

    const COSMETIC_CAPABILITIES: ComponentCapabilities = ComponentCapabilities {
        persistence: PersistenceCapability::None,
        rollback: RollbackCapability::Excluded,
        witness: WitnessCapability::Unwatched,
        replication: ReplicationCapability::None,
        write_authority: WriteAuthorityCapability::Local,
    };

    /// The one question D-3 asks a declaration, in both directions.
    #[test]
    fn persistence_is_read_from_the_declared_p_dimension() {
        assert!(CORE_CAPABILITIES.is_persisted());
        assert!(!COSMETIC_CAPABILITIES.is_persisted());
        assert!(
            ComponentCapabilities {
                persistence: PersistenceCapability::Critical,
                ..COSMETIC_CAPABILITIES
            }
            .is_persisted()
        );
    }

    /// ADR-0045 clause (d)'s five named profiles, and the `CoreClass` names
    /// derived from them — including the two rows that have none.
    #[test]
    fn profiles_derive_the_core_class_vocabulary() {
        assert_eq!(profile_of(CORE_CAPABILITIES), Some(CapabilityProfile::Core));
        assert_eq!(
            profile_of(CORE_CAPABILITIES).and_then(CapabilityProfile::core_class),
            Some(CoreClass::Core)
        );

        let bulk = ComponentCapabilities {
            rollback: RollbackCapability::Excluded,
            witness: WitnessCapability::InvariantChecked,
            ..CORE_CAPABILITIES
        };
        assert_eq!(profile_of(bulk), Some(CapabilityProfile::Bulk));
        assert_eq!(
            profile_of(bulk).and_then(CapabilityProfile::core_class),
            Some(CoreClass::Bulk)
        );

        assert_eq!(
            profile_of(COSMETIC_CAPABILITIES),
            Some(CapabilityProfile::CosmeticLocal)
        );
        assert_eq!(
            profile_of(COSMETIC_CAPABILITIES).and_then(CapabilityProfile::core_class),
            Some(CoreClass::Cosmetic)
        );

        let ephemeral = ComponentCapabilities {
            replication: ReplicationCapability::InterestReplicated,
            write_authority: WriteAuthorityCapability::IslandWeak,
            ..COSMETIC_CAPABILITIES
        };
        assert_eq!(
            profile_of(ephemeral),
            Some(CapabilityProfile::EphemeralShared)
        );
        let ledger = ComponentCapabilities {
            persistence: PersistenceCapability::Critical,
            write_authority: WriteAuthorityCapability::ClusterTransaction,
            ..COSMETIC_CAPABILITIES
        };
        assert_eq!(profile_of(ledger), Some(CapabilityProfile::CriticalLedger));
        // The demonstration that the enum could not carry the space: these
        // two profiles have no `CoreClass` name at all.
        assert_eq!(
            profile_of(ephemeral).and_then(CapabilityProfile::core_class),
            None
        );
        assert_eq!(
            profile_of(ledger).and_then(CapabilityProfile::core_class),
            None
        );
    }

    /// A typo'd dimension lands on no named profile — A5 §6.2's tripwire.
    #[test]
    fn an_unnamed_capability_combination_has_no_profile() {
        assert_eq!(
            profile_of(ComponentCapabilities {
                write_authority: WriteAuthorityCapability::Local,
                ..CORE_CAPABILITIES
            }),
            None
        );
    }

    /// The manifest answers for the components it declares, and fails closed
    /// for the ones it does not.
    #[test]
    fn declaration_lookup_fails_closed_for_undeclared_components() {
        const COMPONENT_SCHEMAS: &[ComponentSchemaManifest] = &[ComponentSchemaManifest {
            owner: MODULE_A,
            id: ComponentSchemaId {
                component: regolith::STATE,
                version: orrery_protocol::atrest::SCHEMA_V0,
            },
            capabilities: CORE_CAPABILITIES,
        }];
        const MODULES: &[ModuleManifest] = &[MODULE_A_MANIFEST];
        let manifest = manifest(MODULES, COMPONENT_SCHEMAS, EMPTY_AMBIGUITIES);
        assert_eq!(
            manifest
                .declaration(regolith::STATE)
                .map(|schema| schema.capabilities),
            Some(CORE_CAPABILITIES)
        );
        assert_eq!(manifest.declaration(ComponentTypeId(0xDEAD_BEEF)), None);
    }

    #[test]
    fn canonical_schedule_rejects_ambiguity() {
        const MODULES: &[ModuleManifest] = &[MODULE_A_MANIFEST];
        let error = validate(&manifest(MODULES, EMPTY_COMPONENTS, AMBIGUITIES));
        assert_eq!(
            error,
            Err(CompositionError::CanonicalScheduleAmbiguity(AMBIGUITIES[0]))
        );
    }
}
