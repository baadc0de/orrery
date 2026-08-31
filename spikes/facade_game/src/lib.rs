//! Spike (#793): a stand-in game crate that cannot name `bevy_ecs`.
//!
//! **Propose-only.** This crate exists to be compiled. Its `Cargo.toml`
//! declares the facade under the key `bevy_ecs`, so within this crate
//! `bevy_ecs::…` means the facade and upstream `bevy_ecs` — a transitive
//! dependency — is not in the extern prelude and cannot be named at all.
//!
//! Everything below either compiles, which is the claim, or is a
//! `compile_fail` doctest, which is the refusal. Both are run by
//! `cargo test -p facade_game`.

use orrery_protocol::PersistId;

/// `#[derive(Component)]` in a crate that cannot name `bevy_ecs`.
///
/// This is question 1 answered in four lines. The expansion emits paths into
/// `bevy_ecs::component`, `bevy_ecs::entity`, `bevy_ecs::lifecycle`,
/// `bevy_ecs::relationship` and `bevy_ecs::world`; the manifest key sends all
/// of them to the facade, and the facade re-exports the first four. (It does
/// **not** re-export `world`, and `#[derive(Component)]` still compiles —
/// `world` is reached only by a component that declares hooks.)
///
/// #804 observed that `#[derive(Component)] struct Rock` deletes #798's
/// argument that `Query<&Rock>` could not be written. It does; this line is
/// that observation made concrete.
#[derive(bevy_ecs::Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rock {
    /// Remaining integrity.
    pub hp: u32,
}

/// A second component, so a query can discriminate.
#[derive(bevy_ecs::Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Occluder;

/// A game-authored system that reaches one named neighbour.
///
/// The shape `visibility.rs:171` has today, written as a query. The id arrives
/// from the signed input log — `Order::ClaimCover` / `Order::Collide` — so the
/// rule still *names* what it wants; what changed is that the search is
/// recorded whether or not it lands.
///
/// Returns whether the named rock was found, so a test can assert that the
/// answer and the log disagree about nothing.
pub fn read_named_neighbour(
    rocks: &mut bevy_ecs::OrderedQuery<&'static Rock>,
    wanted: PersistId,
) -> Option<u32> {
    rocks.get(wanted).map(|rock| rock.hp)
}

/// A game-authored system that asks who is nearby.
///
/// The thing `neighbor(id)` cannot express and a query can. Every yielded id
/// is recorded, so branching on the population is recorded too — which is the
/// half `Query::iter().count()` gets for free today.
pub fn count_occluders(occluders: &mut bevy_ecs::OrderedQuery<&'static Occluder>) -> usize {
    occluders.enumerate().len()
}

/// `Query` cannot be named through the facade.
///
/// ```compile_fail
/// fn f(_q: bevy_ecs::system::Query<'_, '_, &'static facade_game::Rock>) {}
/// ```
///
/// `World` cannot be named through the facade, so `World::get`,
/// `World::entity` and `World::query` are all out of reach with it.
///
/// ```compile_fail
/// fn f(_w: &bevy_ecs::world::World) {}
/// ```
///
/// Neither can `Commands`, `EntityRef` or `UnsafeWorldCell`.
///
/// ```compile_fail
/// fn f(_c: bevy_ecs::system::Commands<'_, '_>) {}
/// ```
/// ```compile_fail
/// fn f(_e: bevy_ecs::world::EntityRef<'_>) {}
/// ```
/// ```compile_fail
/// fn f(_u: bevy_ecs::world::unsafe_world_cell::UnsafeWorldCell<'_>) {}
/// ```
///
/// And upstream cannot be reached under its own name either, because a
/// transitive dependency is not in the extern prelude. This is the line that
/// makes the facade a boundary rather than a suggestion: there is no spelling
/// of upstream available in this crate at all.
///
/// ```compile_fail
/// use ::bevy_ecs::system::Query;
/// ```
///
/// What *does* compile is the curated surface.
///
/// ```
/// #[derive(bevy_ecs::Component)]
/// struct Pickup { value: u32 }
/// let _ = core::mem::size_of::<bevy_ecs::Entity>();
/// ```
pub mod refusals {}

/// The `SystemParam` derive is re-exported by the facade and still does not
/// compile here, because its expansion names `bevy_ecs::world::World` and the
/// facade exposes no `world` module.
///
/// This is question 2's answer in one test. Curating `Query` out is not the
/// hard part; `World` is, because it is inherently a door to every component
/// of every entity and because the derive *requires* it to be nameable. A
/// facade cannot both admit `#[derive(SystemParam)]` in game crates and
/// withhold `World`. So `OrderedQuery` is defined in the facade instead, where
/// `World` is already in reach, and game crates consume it without ever
/// deriving a system param of their own.
///
/// ```compile_fail
/// #[derive(bevy_ecs::SystemParam)]
/// struct Store<'w> {
///     rocks: bevy_ecs::OrderedQuery<'w, 'w, &'static facade_game::Rock>,
/// }
/// ```
pub mod system_param_is_refused {}

/// The door a name allowlist does not close.
///
/// `bevy_ecs::lifecycle::ComponentHook` is a type alias for
/// `for<'w> fn(DeferredWorld<'w>, HookContext)`. Re-exporting the *alias* —
/// which looks inert, and which a reviewer scanning for `World` would pass
/// over — hands a game crate a `DeferredWorld` value by type inference, and
/// `DeferredWorld::get` reaches any component of any entity. This compiled:
///
/// ```ignore
/// const HOOK: bevy_ecs::lifecycle::ComponentHook = |mut world, ctx| {
///     let _ = world.get::<Rock>(ctx.entity);
///     let _ = world.entity(ctx.entity).get::<Rock>();
/// };
/// ```
///
/// The facade now omits `lifecycle`, so it does not:
///
/// ```compile_fail
/// const HOOK: bevy_ecs::lifecycle::ComponentHook = |_w, _c| {};
/// ```
///
/// The general rule is in the facade's source: an allowlist must be closed
/// under the types reachable through the signatures of the items on it, not
/// merely over the names it writes down.
pub mod the_alias_door {}
