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

`--render-smoke` is the bounded rendered launch proof used by the release
workflow. It keeps the windowed client alive for twenty seconds, then requests a
primary-window screenshot and exits successfully only when Bevy reports that
the renderer completed that frame. It exits with an error after 60 seconds if
the renderer never produces the screenshot. This distinguishes process spawn
from a client that remains alive and renders, including delayed compositor
failures.

## Verifying presentation without a desktop

Presentation issues (#524, #530, #531) sat blocked for want of a way to see the
game. Two things unblock it, and the order between them matters: **measure
first, look second.** A measurement is reproducible in CI and a screenshot is
not, so anything a number can settle stays settled.

`--capture-geometry` prints the rock census once a second, and one line per
frame an impact burst is live:

```
rocks 1 L / 2 M / 3 S in state | 6 drawn | 6 in view | tier px L 17.4 / M 8.7 /
  S 3.5 | smallest drawn 3.5 px BELOW THE 4 px FLOOR | camera 4000 m / 720 px
impact_capture target=1 progress=0.00 | burst 1.65 m = 0.7 px shown=true |
  marker 64.00 m = 27.8 px shown=true | cue 27.8 px | camera 4000 m / 720 px
```

Every number on the rock line comes from a different place -- the executor, the
`RockBody` entities, Bevy's own `ViewVisibility`, and the drawn body's own mesh
-- so the line says *which* stage lost a rock instead of leaving it to be
guessed. `--capture-zoom-sweep` walks the camera between its limits, and
`--capture-frames <dir>` writes a PNG of the rendered frame at each zoom
extreme, plus one at each extreme while a confirmed hit is bursting.

The frames come from Bevy's screenshot path -- the texture the renderer
presented -- so no compositor has to be readable for them to be captured.

**The client runs headless under Xvfb, on the real GPU.** Two things are
required, and the second is what earlier attempts missed:

```sh
Xvfb :99 -screen 0 1920x1080x24 -ac +extension GLX +render -noreset &
env -u WAYLAND_DISPLAY WINIT_UNIX_BACKEND=x11 DISPLAY=:99 \
  cargo run -- --campaign --campaign-consent --host-node <node> \
    --host-direct <ip:port> --slot <n> \
    --capture-geometry --capture-zoom-sweep --capture-frames out/frames
```

Without `-u WAYLAND_DISPLAY`, winit prefers Wayland over the `DISPLAY` just
set, attaches to the desktop session, and the window is torn down within
seconds -- which reads as an unexplained crash rather than as a wrong backend.
With it, wgpu still selects the discrete GPU, so this is not a software
rendering path and what is captured is what a player would see.

A host to join is `gates/p1-swarm --external-peer --peers 8 --min-cells 1
--witness --listening-file listening.txt`, which seeds the campaign rock
pocket. Without `--issuer-key` it admits the deterministic slot identity, so
point `--identity-file` at a file holding `bot_key(peers)`.
