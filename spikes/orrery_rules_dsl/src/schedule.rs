//! Deriving the declared schedule from the table that runs.
//!
//! # The gap this addresses, quoted from the tree
//!
//! `crates/orrery_games/src/regolith/mod.rs:864-874`, on
//! `REGOLITH_CANONICAL_SCHEDULE`:
//!
//! > It is written out rather than derived because `CanonicalSchedule` is a
//! > `const` of `&'static` slices and the runnable table holds function
//! > pointers; deriving one from the other in a `const` context is not
//! > expressible today. The two are held together by
//! > `schedule_tests::the_declared_schedule_matches_the_table_that_runs`.
//!
//! That is accurate about *deriving one from the other*, and it is the reason
//! D43 clause (g)'s digest currently covers a table a test says matches the one
//! that runs. A macro does not have to derive either from the other: it emits
//! **both from one source**, so there is nothing left to disagree. The test
//! becomes vacuous rather than load-bearing, and the same is true of
//! `Schedule::duplicate_system_name` — which the expansion turns into a
//! compile error.
//!
//! # And the second admission, two paragraphs later
//!
//! > D43 clause (c)(1) asks for an explicit edge on *every* pair with
//! > conflicting data access, derived mechanically; that derivation needs
//! > per-system data-access declarations, which this design does not yet carry.
//!
//! [`canonical_schedule!`] does not carry them either, and that is stated in
//! its own docs rather than implied away. What it does carry is the *checking*
//! of the hand-written subset: a declared edge whose `before` does not precede
//! its `after` in the runnable order is a compile error, not a test failure.
//! Mechanically deriving the full conflict set is the piece that needs a
//! procedural macro — see `docs/spikes/gameplay-macro-dsl.md` §6.

/// `&str` equality, in a `const` context.
///
/// `PartialEq for str` is not `const`, and the whole point of this module is to
/// move the schedule's agreement checks from a test to const evaluation.
#[must_use]
pub const fn str_eq(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    if left.len() != right.len() {
        return false;
    }
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

/// The index of `needle` in `names`, or `names.len()` when it is absent.
///
/// Returning the length rather than `Option` keeps the edge check a single
/// comparison: an absent `before` sorts after everything and fails the edge,
/// which is the verdict an undeclared system deserves anyway.
#[must_use]
pub const fn position_or_end(names: &[&str], needle: &str) -> usize {
    let mut index = 0;
    while index < names.len() {
        if str_eq(names[index], needle) {
            return index;
        }
        index += 1;
    }
    names.len()
}

/// Whether any name appears twice.
///
/// Two systems sharing one name makes an ordering edge ambiguous and the
/// schedule digest a statement about something that does not exist — the
/// reasoning `Schedule::duplicate_system_name` already carries. The difference
/// is that this one runs at compile time.
#[must_use]
pub const fn has_duplicate(names: &[&str]) -> bool {
    let mut outer = 0;
    while outer < names.len() {
        let mut inner = outer + 1;
        while inner < names.len() {
            if str_eq(names[outer], names[inner]) {
                return true;
            }
            inner += 1;
        }
        outer += 1;
    }
    false
}

/// Whether every declared edge agrees with the canonical order in `names`.
///
/// An edge naming a system that is not in the table fails, and so does an edge
/// whose endpoints are the same system.
#[must_use]
pub const fn edges_are_ordered(names: &[&str], edges: &[(&str, &str)]) -> bool {
    let mut index = 0;
    while index < edges.len() {
        let (before, after) = edges[index];
        let before = position_or_end(names, before);
        let after = position_or_end(names, after);
        if before >= names.len() || after >= names.len() || before >= after {
            return false;
        }
        index += 1;
    }
    true
}

/// Emit a ruleset's runnable schedule and its declared manifest from one
/// source, and check their agreement at compile time.
///
/// # Form
///
/// ```ignore
/// canonical_schedule! {
///     rules:    Demo,
///     locals:   DemoLocals,
///     runnable: pub DEMO_SCHEDULE,
///     declared: pub DEMO_CANONICAL,
///     observe:  "observe" => [ "verify-claims" => CLAIM_READS ],
///     stages:   [
///         "control" => [
///             "tick-cooldowns" => tick_cooldowns,
///             "fire"           => fire,
///         ],
///         "motion" => [ "integrate" => integrate ],
///     ],
///     edges: [ "tick-cooldowns" -> "fire", "fire" -> "integrate" ],
/// }
/// ```
///
/// Each system is `"canonical-name" => body`, where `body` is an
/// [`orrery_core::SystemFn`] — which is what a `projected_system!`-shaped macro
/// should yield once the name lives here instead of beside the body. The
/// observation entries are `"canonical-name" => OBSERVATION`, taking an
/// [`orrery_core::Observation`] such as the one [`recorded_reads!`] generates;
/// the name is checked against the observation's own at compile time, so the
/// two cannot drift.
///
/// # What is a compile error rather than a test
///
/// * A system name appearing twice anywhere in the tick.
/// * A declared ordering edge whose `before` does not precede its `after` in
///   the runnable order, or either of whose endpoints is not a system.
/// * An observation whose declared name differs from the name it carries.
/// * Any disagreement between the runnable stage/system table and the declared
///   one — structurally impossible, since one list produces both.
///
/// # What is still hand-written
///
/// The `edges:` list. Deriving it needs per-system read/write declarations
/// (D43 clause (c)(1)); see this module's own docs. `ambiguities` is empty and
/// `executor_policy` is [`SingleThreaded`](orrery_compose::ExecutorPolicy) for
/// the same reason Regolith's are: a list is already a total order.
///
/// [`recorded_reads!`]: crate::recorded_reads
#[macro_export]
macro_rules! canonical_schedule {
    (
        rules:    $rules:ty,
        locals:   $locals:ty,
        runnable: $rvis:vis $runnable:ident,
        declared: $dvis:vis $declared:ident,
        observe:  $obs_stage:literal => [
            $( $obs_name:literal => $obs:expr ),* $(,)?
        ],
        stages: [
            $(
                $stage:literal => [
                    $( $sys_name:literal => $sys:expr ),* $(,)?
                ]
            ),* $(,)?
        ],
        edges: [ $( $before:literal -> $after:literal ),* $(,)? ] $(,)?
    ) => {
        /// Every canonical system name in execution order, observation stage
        /// first. One list; both tables below are built from it.
        const CANONICAL_SYSTEM_NAMES: &[&str] =
            &[ $( $obs_name, )* $( $( $sys_name, )* )* ];

        /// The hand-written ordering edges, checked below.
        const CANONICAL_ORDERING_EDGES: &[(&str, &str)] =
            &[ $( ($before, $after) ),* ];

        // The agreement checks D43 clause (g) needs, at compile time. Each of
        // these was a runtime test the game had to remember to write.
        const _: () = {
            ::core::assert!(
                !$crate::schedule::has_duplicate(CANONICAL_SYSTEM_NAMES),
                "two canonical systems share one name: an ordering edge \
                 addressing it is ambiguous and the schedule digest describes \
                 something that does not exist",
            );
            ::core::assert!(
                $crate::schedule::edges_are_ordered(
                    CANONICAL_SYSTEM_NAMES,
                    CANONICAL_ORDERING_EDGES,
                ),
                "a declared ordering edge disagrees with the table that runs, \
                 or names a system the table does not contain",
            );
            $(
                ::core::assert!(
                    $crate::schedule::str_eq($obs_name, $obs.name.0),
                    "an observation is declared under a different name than \
                     the one it carries",
                );
            )*
        };

        /// The runnable tick.
        $rvis static $runnable: ::orrery_core::Schedule<$rules, $locals> =
            ::orrery_core::Schedule {
                observe_stage: ::orrery_core::StageName($obs_stage),
                observe: &[ $( $obs ),* ],
                stages: &[
                    $(
                        ::orrery_core::Stage {
                            name: ::orrery_core::StageName($stage),
                            systems: &[
                                $(
                                    ::orrery_core::System {
                                        name: ::orrery_core::SystemName($sys_name),
                                        run: $sys,
                                    },
                                )*
                            ],
                        },
                    )*
                ],
            };

        /// The declared topology D43 clause (g) digests, from the same source.
        $dvis const $declared: ::orrery_compose::CanonicalSchedule =
            ::orrery_compose::CanonicalSchedule {
                stages: &[
                    ::orrery_compose::ScheduleStageManifest {
                        id: ::orrery_compose::ScheduleStageId($obs_stage),
                        systems: &[
                            $( ::orrery_compose::SystemId($obs_name), )*
                        ],
                    },
                    $(
                        ::orrery_compose::ScheduleStageManifest {
                            id: ::orrery_compose::ScheduleStageId($stage),
                            systems: &[
                                $( ::orrery_compose::SystemId($sys_name), )*
                            ],
                        },
                    )*
                ],
                ordering_edges: &[
                    $(
                        ::orrery_compose::ScheduleOrderingEdge {
                            before: ::orrery_compose::SystemId($before),
                            after: ::orrery_compose::SystemId($after),
                        },
                    )*
                ],
                // A list is already a total order, so there is nothing for
                // ambiguity detection to find — Regolith's own reasoning.
                ambiguities: &[],
                ambiguity_detection: ::orrery_compose::AmbiguityDetection::Error,
                executor_policy: ::orrery_compose::ExecutorPolicy::SingleThreaded,
            };
    };
}
