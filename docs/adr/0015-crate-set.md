# ADR-0015: Crate set

**Status:** Accepted · **Date:** 2026-08-11 · **Decision:** D15

This decision is normative. See the [ADR index](../DECISIONS.md) for precedence, scope, and the complete decision set.

Engine-agnostic core (no Bevy dependency): `orrery_protocol`, `orrery_core`. Server binaries are Bevy-free except `orrery_field_host`.

| Crate | Kind | Responsibility |
|---|---|---|
| `orrery_protocol` | lib | Wire & data types: `CellId`, intents, leases, attestations, evidence bundles, log frames/state claims; canonical scalars (`Tick` = u64 universe ticks, `PersistId` = u64 cluster-minted, `CellId` = NonZeroU64, `RulesetId` = version + build digest); postcard/bitcode encoding; versioning. |
| `orrery_aeronet_iroh` | lib | iroh IO layer for `aeronet_io` (upstream candidate). |
| `orrery_net` | Bevy plugin | Session lifecycle: coordinator client, island membership, peer connect/disconnect, channel policy (datagrams=state, streams=control/bulk), relay-path telemetry. |
| `orrery_spatial` | Bevy plugin | `CellId` math + `big_space` integration; AOI subscription (27-cell), replicon visibility mapping, interest-set selection, hysteresis, proxy extrapolation. |
| `orrery_authority` | Bevy plugin | Weak/strong claims, sequence numbers, optimistic lease client, handoff, orphan recovery, contact-island propagation. |
| `orrery_predict` | Bevy plugin | lightyear configuration for per-entity authority; reconciliation-error monitor (witness signal); rollback budget guard. |
| `orrery_core` | lib | Verifiable core: `Ruleset` trait, fixed-tick executor, deterministic RNG, quantization, tolerance comparators, signed hash-chained input logs, replay harness (headless). |
| `orrery_witness` | Bevy plugin | Invariant validators, discrepancy detection/reports, attestation co-signing, evidence assembly. |
| `orrery_persist_client` | Bevy plugin | Gateway session, area load/subscribe, diff uplink scheduler, intent submission + offline queue, prediction of intent outcomes. |
| `orrery_persistd` | lib+bin harness | Cell actors, journal, FDB checkpoint/restore, lease registrar, gateway, intent validation, adjudication executor, hotspot splitting. |
| `orrery_coordinator` | bin | Presence, islands, witness seeding, promotion, field-host scheduling. |
| `orrery_identity` | bin | Accounts, tokens, strikes, bans. |
| `orrery_field_host` | bin | Headless Bevy authority host (promoted cells, witness fallback, parked-cell catch-up execution). |

Dependency spine: `protocol` ← everything; `core` ← {witness, persistd, field_host, game}; client plugins compose as a `OrreryClientPlugins` group.

