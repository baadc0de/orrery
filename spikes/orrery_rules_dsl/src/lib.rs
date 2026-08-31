//! **Spike (#793), propose-only.** A macro-defined form for gameplay rules in
//! which the adjudication guarantees are *structural* — properties of what an
//! author can write down at all, rather than checks bolted on after the body
//! exists.
//!
//! Nothing here is wired into a shipping crate. `orrery_core` is not amended,
//! `Ruleset::step`'s signature is untouched, Regolith is not migrated, and
//! `scripts/core-gates.sh` is unchanged. The write-up is
//! `docs/spikes/gameplay-macro-dsl.md`.
//!
//! # What this completes
//!
//! Two macros already in the tree pointed here:
//!
//! * [`projected_system!`](../../../crates/orrery_games/src/regolith/mod.rs)
//!   takes the *projection* half of an ECS query and leaves the selection —
//!   "the runner never scans a population, so a rule cannot reach a neighbour
//!   it did not record".
//! * `section_invariant!` (`orrery_core::invariants`) lifts a per-section check
//!   into the whole-state `Invariant` a ruleset publishes, and refuses a check
//!   written for another section at the expansion site by annotating a local
//!   `fn` pointer rather than inferring it.
//!
//! Both of them stop one step short of the read itself. An
//! [`orrery_core::Observation`] is still handed a `&mut StateView`, so the
//! recorded read is confined to a *signature* but not to a *shape*: the body
//! may call [`StateView::neighbor`](orrery_core::StateView::neighbor) any
//! number of times, on any identifier, in any order, and the ruleset's
//! [`max_neighbor_reads`](orrery_core::Ruleset::max_neighbor_reads) is a
//! separately hand-written number that must be kept in agreement by hand.
//!
//! [`recorded_reads!`] closes that last step. It splits an observation into two
//! author-written pure functions, neither of which is handed a `StateView`:
//!
//! ```text
//! resolve : (reader, &own, &inputs)                            -> Targets  // who to read
//! apply   : (reader, &own, &Targets, &Frames, &inputs, &mut L) -> ()       // what came back
//! ```
//!
//! The applier is handed the targets as well as the frames because an emitted
//! outcome often has to *name* a neighbour: Regolith's
//! `Outcome::LockVisibility { locker, .. }` carries the locking craft's
//! `PersistId`, which no amount of its state can supply. The targets are plain
//! `Option<PersistId>`s and carry no capability, so this widens what an applier
//! can say without widening what it can read.
//!
//! and generates the only expression between them:
//!
//! ```text
//! Frames { slot_n: targets.slot_n.and_then(|id| view.neighbor(id).cloned()), .. }
//! ```
//!
//! # The guarantees, and why each is structural
//!
//! | Guarantee | Mechanism |
//! |---|---|
//! | Every neighbour read goes through the recorded path | Neither author function can name a `StateView`; the annotated `fn` pointers in the expansion refuse a signature that does. |
//! | The read is stamped with reader identity | `reader` is `view.entity()`, supplied by the expansion. An author cannot fabricate it, and `StateView::neighbor` refuses it by identity (`ruleset.rs:176`). |
//! | Staleness is applied at the read | The read *is* `StateView::neighbor`, so `ruleset.rs:192`'s `checked_sub` is the one implementation, not a second one that has to agree. |
//! | There is no way to name a neighbour except through that path | An author holds `Option<PersistId>`s and `Option<CoreState>`s. There is no handle from either to the store. |
//! | Ordering is established, not inherited | Slot order is declaration order, fixed in the generated struct literal. A resolver returns a fixed-shape record, so it cannot reorder, grow or shrink the read set. |
//! | The read cap is derived, not restated | [`RecordedReads::MAX_NEIGHBOR_READS`] is the slot count. A ruleset writes `fn max_neighbor_reads(&self) -> usize { Frames::MAX_NEIGHBOR_READS }` and the number cannot drift from the reads. |
//! | Engine handles are unrepresentable | The whole author-facing vocabulary is `PersistId`, `CoreState`, `CoreInput` and the game's own `Locals`. No `World`, no `StateView`, no store handle is reachable through any of them. |
//!
//! # What it does *not* do, stated first
//!
//! * **It is opt-in.** `Ruleset::step` still takes `&mut StateView` and
//!   `StateView::neighbor` is still `pub`, so a game that declines the macro
//!   reads however it likes. The macro raises the floor for code that uses it
//!   and does nothing about code that does not. Closing that needs
//!   `orrery_core` to narrow its surface — owed work, not spike work.
//! * **It does not prevent #820's divergences,** because in this tree they do
//!   not exist. Both were properties of a *second* read path
//!   (`OrderedQuery::get`) that was not told who was reading or when.
//!   `StateView::neighbor` is told both. What the macro changes is that a
//!   second read path stops being reachable from a rule body at all.
//! * **Dependent reads are not expressible inside one observation.** A read
//!   whose target depends on an earlier read of the same observation has
//!   nowhere to go, because `resolve` runs before any read happens. It is
//!   expressible across *two* declared observations, whose caps sum — which is
//!   the honest form, since the chained read is a distinct recorded fact.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod schedule;

/// The compile-time facts a generated frame record carries about its own
/// observation.
///
/// Implemented only by the `Frames` type [`recorded_reads!`] generates. It is
/// the seam that makes a ruleset's declared cap *derived*: `max_neighbor_reads`
/// reads it off the type instead of restating a number beside it.
pub trait RecordedReads {
    /// Distinct neighbours this observation can read in one step.
    ///
    /// Exactly the declared slot count. A slot may resolve to `None` and read
    /// nothing, and two slots may name the same identifier — in which case
    /// `StateView` records it once — so this is an upper bound, which is what
    /// [`orrery_core::Ruleset::max_neighbor_reads`] asks for.
    const MAX_NEIGHBOR_READS: usize;

    /// Slot names in read order, for review and for the schedule digest.
    ///
    /// This is the list `scripts/core-gates.sh` clause 5's
    /// `AUDITED_NEIGHBOR_PREDICATES` approximates with a `grep` for
    /// `view.neighbor(` and an `awk` for the nearest enclosing `fn`. Here it is
    /// the declaration itself rather than a scan of the text around it.
    const SLOT_NAMES: &'static [&'static str];
}

/// Declare an audited neighbour-reading observation whose read set is data.
///
/// # Form
///
/// ```ignore
/// recorded_reads! {
///     /// Doc comment lands on the generated `Observation` constant.
///     pub CLAIM_READS {
///         rules:   Regolith,
///         locals:  RegolithLocals,
///         system:  "verify-claims",
///         targets: ClaimTargets,
///         frames:  ClaimFrames,
///         slots:   [
///             /// The craft whose lock a cover claim challenges.
///             cover_locker,
///             /// The rock a cover claim names as occluder.
///             cover_rock,
///             /// The counterparty a collision claim names.
///             collision,
///         ],
///         resolve: claim_targets,
///         apply:   apply_claims,
///     }
/// }
/// ```
///
/// # What it generates
///
/// * `struct $targets` — one `Option<PersistId>` field per slot. The resolver's
///   whole return value.
/// * `struct $frames` — one `Option<CoreState>` field per slot, plus an
///   [`impl RecordedReads`] carrying the cap and the slot names.
/// * `const $name: Observation<$rules, $locals>` — the runnable observation,
///   ready to place in a [`orrery_core::Schedule`]'s `observe` list.
///
/// # Why the two `let` bindings in the expansion are annotated
///
/// The same reason `section_invariant!` annotates its `check` local: an
/// inferred binding would accept any callable and report the mismatch far from
/// the declaration, or not at all. Annotated, the resolver and the applier are
/// checked against exactly one signature at the macro call site — and that
/// signature is the enforcement. A `resolve` that asks for a `&mut StateView`,
/// returns the wrong record, or is written for another ruleset's state is a
/// type error on the line that declared it.
///
/// # Slot order
///
/// Declaration order is read order, because the generated struct literal names
/// the fields in that order and Rust evaluates struct-literal fields in written
/// order. Reordering the `slots:` list is therefore a canonical change with a
/// visible one-line diff, which is the same property
/// `orrery_core::Schedule`'s `const` table gives system order.
#[macro_export]
macro_rules! recorded_reads {
    (
        $(#[$meta:meta])*
        $vis:vis $name:ident {
            rules:   $rules:ty,
            locals:  $locals:ty,
            system:  $system:literal,
            targets: $targets:ident,
            frames:  $frames:ident,
            slots:   [ $( $(#[$slot_meta:meta])* $slot:ident ),+ $(,)? ],
            resolve: $resolve:expr,
            apply:   $apply:expr $(,)?
        }
    ) => {
        /// Which neighbours this observation will read, one field per declared
        /// slot. Produced by the resolver, consumed only by the generated read
        /// expression.
        ///
        /// `None` reads nothing and is framed absent, which is a recorded fact
        /// and not a silence: `StateView` still logs a slot the resolver
        /// declined only if the resolver named an identifier, so declining is
        /// visible in the frame set by the read's absence.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
        $vis struct $targets {
            $(
                $(#[$slot_meta])*
                pub $slot: ::core::option::Option<::orrery_protocol::PersistId>,
            )+
        }

        /// What the recorded read returned, one field per declared slot.
        ///
        /// `None` covers all four ways a slot can be empty and deliberately
        /// does not distinguish them: the resolver named nobody, the neighbour
        /// is not in the snapshot, the identifier was the reader's own, or the
        /// observation was staler than
        /// [`orrery_core::Ruleset::max_neighbor_staleness_ticks`]. A rule that
        /// could tell a hidden row from an absent one would be reading a fact
        /// the log does not carry.
        #[derive(Debug, Clone, PartialEq, Eq)]
        $vis struct $frames {
            $(
                $(#[$slot_meta])*
                pub $slot: ::core::option::Option<
                    <$rules as ::orrery_core::Ruleset>::CoreState,
                >,
            )+
        }

        impl $crate::RecordedReads for $frames {
            const MAX_NEIGHBOR_READS: usize = 0usize $( + $crate::__slot_unit!($slot) )+;
            const SLOT_NAMES: &'static [&'static str] = &[ $( ::core::stringify!($slot) ),+ ];
        }

        $(#[$meta])*
        $vis const $name: ::orrery_core::Observation<$rules, $locals> =
            ::orrery_core::Observation {
                name: ::orrery_core::SystemName($system),
                run: {
                    fn run(
                        view: &mut ::orrery_core::StateView<
                            '_,
                            <$rules as ::orrery_core::Ruleset>::CoreState,
                        >,
                        inputs: &::orrery_core::OrderedInputs<
                            '_,
                            <$rules as ::orrery_core::Ruleset>::CoreInput,
                        >,
                        locals: &mut $locals,
                    ) {
                        // Annotated, never inferred. These two lines are the
                        // enforcement: a resolver or applier that asks for a
                        // `StateView`, for another ruleset's state, or for a
                        // different record shape fails here, at the
                        // declaration, and not somewhere downstream.
                        let resolve: fn(
                            ::orrery_protocol::PersistId,
                            &<$rules as ::orrery_core::Ruleset>::CoreState,
                            &::orrery_core::OrderedInputs<
                                '_,
                                <$rules as ::orrery_core::Ruleset>::CoreInput,
                            >,
                        ) -> $targets = $resolve;
                        let apply: fn(
                            ::orrery_protocol::PersistId,
                            &<$rules as ::orrery_core::Ruleset>::CoreState,
                            &$targets,
                            &$frames,
                            &::orrery_core::OrderedInputs<
                                '_,
                                <$rules as ::orrery_core::Ruleset>::CoreInput,
                            >,
                            &mut $locals,
                        ) = $apply;

                        // Supplied by the executor through the view, never by
                        // the state: a rule cannot claim to be an entity it is
                        // not, and cannot decline to be stamped.
                        let reader = ::orrery_core::StateView::entity(view);
                        let targets = resolve(reader, view.own(), inputs);

                        // The one read expression. It is the only place in a
                        // game written this way where `view.neighbor` appears,
                        // and a rules author has no `view` in scope to write a
                        // second one. Field order is declaration order, so the
                        // recorded first-read order is the declared slot order
                        // and not whatever the resolver felt like.
                        let frames = $frames {
                            $(
                                $slot: targets
                                    .$slot
                                    .and_then(|id| view.neighbor(id).cloned()),
                            )+
                        };

                        apply(reader, view.own(), &targets, &frames, inputs, locals);
                    }
                    run
                },
            };
    };
}

/// One, for counting slots in a repetition. Not public API.
#[doc(hidden)]
#[macro_export]
macro_rules! __slot_unit {
    ($slot:ident) => {
        1usize
    };
}
