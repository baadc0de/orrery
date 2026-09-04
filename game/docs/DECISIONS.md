# Mothership — Game Design Decision Records

**Status:** Accepted design · **Initial decision date:** 2026-09-03 · **Naming:** *Mothership* is a provisional working title.

*Mothership* is a game built on Orrery. Its design decisions are kept as independent records under [`game/docs/adr/`](adr/), separate from [Orrery's architecture trail](../../docs/DECISIONS.md). The two trails cite each other: a game ADR states what the game needs, and an Orrery ADR states how the framework changes to provide it. Where a game document conflicts with an accepted game ADR, the ADR wins; where a game requirement conflicts with an Orrery ADR, an Orrery ADR is filed to resolve it.

## Decision index

| Decision | Record | Scope |
|---|---|---|
| GD1 | [Game ADR-0001](adr/0001-requirements.md) | Settled game design requirements G1–G9 |
| GD2 | [Game ADR-0002](adr/0002-client-engine.md) | Unreal 5.8 client, in-process Orrery, cooked season content (G10) |
| GD3 | [Game ADR-0003](adr/0003-spike-outcomes.md) | Host prong, collision representation, playable surface, distribution, CMC, interiors (from spikes #1069–#1072) |

## Documents

| Doc | Scope |
|---|---|
| [00-requirements.md](00-requirements.md) | Game design requirements with engineering consequences |
