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

## Joining with one file

The preferred campaign handoff is a join file created offline by the operator:

```sh
orrery-invite session-token --issuer-credential issuer.cred --account 7 \
  --node "$(orrery-regolith --print-slot-key 3)" \
  --join-file volunteer.join.json --host-node <host-node> --slot 3 \
  --session-id <pre-minted-uuidv7>
orrery-regolith --join volunteer.join.json --host-direct <ip:port> --campaign-consent
```

It is strict named-field JSON with a V1 format marker, carrying `host_node`,
`slot`, `session_id`, and `session_token`; treat it as a short-lived credential.
`--join` overrides all individual launch/token inputs. Without `--join`,
`--session-token <hex>` (or `--session-token @path`) overrides
`ORRERY_SESSION_TOKEN`. The direct argument remains supported for existing
automation, but exposes the token in shell history and process listings; prefer
the join file or `@path` for new sessions.

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
