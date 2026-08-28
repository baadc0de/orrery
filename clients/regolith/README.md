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

## The waiting room

A campaign that has a lobby draws one, top-centre, under the session banner:
the phase the host named, how many player seats are taken, every configured
seat with its state and who holds it, and the countdown — but only when the
host actually sends `starts_in_s`. Nothing here is derived from a local clock,
and an empty seat draws its state word and **no name**, the same absence rule
`src/roster.rs` applies to craft labels (#484). A service that names no phase
describes no lobby, so none is drawn rather than one assembled out of whatever
rows arrived. `src/lobby.rs` is the whole of it.

The room shares the screen with the controls legend rather than preceding it:
the lobby is exactly when a first-time player has nothing to do but read, and
`the_waiting_room_fits_the_default_720_line_window` measures that the two do
not collide at 1280x720.

`--island-seats <n>` states the host's island size on the `--join` path, which
otherwise falls back to `slot + 1`. Admission supplies it automatically from
the campaign listing's `peers + humans` when the service publishes them. The
number matters because the spawn pose is a function of `(slot, island_seats)`:
a human in seat 4 of 8 whose client assumed 5 starts on an orbit the host did
not put it on.

If the host sends a `StartV1` manifest, the client adopts the membership it
names — active seats to replicate to, witness recipients to send frames to —
and **refuses the session outright** if the manifest disagrees with the
tick-zero anchor already signed. That claim cannot be re-signed, so a
disagreement is fatal rather than adjustable.

Controls: Left/Right turn, Up thrusts, Space fires, a left click picks the
target, the mouse wheel zooms, F3 toggles the detailed overlay and F1 shows or
hides the controls legend. The always-on strip and JSONL stream do not depend
on F3.

The legend itself is the answer to #564 — a new player was told none of the
above. It sits in the bottom-right corner, dims each row as that input is
demonstrated, and retires once every flight input has been used (or after 90 s
in the seat). `src/legend.rs` is the whole of it, and its tests press the key
each row names and assert the binding really fires, so a legend that drifts
from the bindings fails rather than lying.

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

`--headless-join <campaign> --nickname <name> --campaign-consent` is the
bounded live admission and transport proof. It reads the campaign list through
the baked admission origin, applies the same compatibility/open predicate as
the buttons in the lobby, requests a seat, and exits successfully only after
the iroh handshake reaches `JoinState::Joined` and at least one joined tick
runs. Adding one or more `--expect-peer <nickname>` options also requires every
named roster slot to exist as a different `RegolithState::Craft` in the local
executor — a roster row by itself cannot satisfy it. The probe exits only after
all named peers have been observed. `--headless-timeout-secs` defaults to 1020
seconds so an arbitrary launch can wait through the deployed campaign's
900-second attempt plus restart.

The release workflow runs three copies through
`scripts/client-campaign-preflight.sh`. Each copy names both others with
repeated `--expect-peer` options; all three must independently report the baked
origin, a joinable listing, admission acceptance, a completed handshake, and
both other replicated crafts. A negative compatibility probe can use
`--expect-admission-refusal client_rev_mismatch`; it sends the binary's actual
embedded revision and succeeds only when admission returns that exact error
name.

`--help` prints the command-line usage before Bevy, identity loading, or window
creation begins.

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
