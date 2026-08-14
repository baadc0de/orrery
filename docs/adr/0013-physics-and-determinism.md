# ADR-0013: Physics & determinism posture

**Status:** Accepted · **Date:** 2026-08-11 · **Decision:** D13

This decision is normative. See the [ADR index](../DECISIONS.md) for precedence, scope, and the complete decision set.

**avian3d** for presentation/gameplay physics (Bevy-native, lightyear integration). Verifiable-core movement/combat uses framework-provided deterministic kinematic character movement + integer combat math ([D9](0009-verifiable-core.md)) — *not* the full physics engine. Contested physics objects (crates, vehicles) replicate under weak-authority contact-island propagation with quantize-both-sides; their persistence writes are bulk-class (not witness-attested) unless the `Ruleset` says otherwise. rapier documented as the alternative. Cross-platform bit determinism of full physics is explicitly **not** assumed anywhere.

