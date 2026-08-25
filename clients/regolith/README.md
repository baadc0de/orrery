# Regolith client

The first rendered Orrery target: a Bevy 0.19 skin over the headless Regolith
rules and executor.

```sh
cargo run -- --telemetry-jsonl target/regolith-client/session.jsonl
```

Campaign entry is explicit. `--campaign` prints the recording notice and exits
without joining until `--campaign-consent` is supplied. Campaign coordinators
provide the human session UUIDv7 and P4 pipeline digest with the session's
witness report; `scripts/p4-ledger.sh append` then validates and stores the
completed `session` row on its normal banking path.

Controls are keyboard-only: Left/Right turn, Up thrusts, Space fires, and F3
toggles the detailed overlay. The always-on strip and JSONL stream do not
depend on F3.

There are deliberately no required assets. If `assets/regolith/craft.glb` is
present it is used as visual geometry; otherwise the client renders Bevy
primitives. Visual geometry never reaches collision or any simulation input.

`--smoke-test` validates that the client plugins and schedules assemble, then
exits without creating a window, GPU adapter, or pipeline. It is deliberately
not a rendering check: a successful line says `graphics were intentionally not
initialized`, while a composition error remains a client failure. Run the
ordinary client to exercise graphics-device capability.
