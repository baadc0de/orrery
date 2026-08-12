# p0-nat-test

A standalone, friend-distributable binary for the **P0 transport spike** of
Orrery ([docs/11-roadmap.md](../docs/11-roadmap.md) §P0). It exercises the
single biggest bet — iroh 1.0.x QUIC with NAT hole punching and relay fallback
([docs/02-networking.md](../docs/02-networking.md) §1, D3) — with no game and no
replication: raw sessions, datagrams, streams, and NAT telemetry, exactly as P0
specifies.

## What it does

Two roles, one binary:

- **`host`** — the rendezvous. Prints its `NodeId`; friends paste it into
  `--peer`. It accepts the incoming connection and runs the per-tick state
  datagram loop.
- **`peer`** — dials the host by `NodeId`, then runs the same loop.

Every pair reports the telemetry the P0 demo criterion cares about:

- **Path state** — `relay` → `direct` when the hole punch lands (the ~90%
  baseline), or stays `relay` for the expected ~10% tail.
- **Datagram delivery** — per-10s `sent` / `received` / `dropped` at the 60 Hz
  P0 stress rate.

## Build

```sh
cargo build --release
# binary: target/release/p0-nat-test
```

Distribute that one binary (plus the relay URL, which is baked in as the
default). It's a normal Rust binary — no install, no runtime, no Bevy.

## Usage

**Host** (one machine, e.g. yours):

```sh
./p0-nat-test --duration-secs 1800
```

Copy the `node_id=...` it prints (or run `./p0-nat-test --print-id`).

**Each friend** (their own NAT):

```sh
./p0-nat-test --peer <host-node-id> --duration-secs 1800
```

Both sides log `path state changed path=direct` when the punch lands and
`datagram stats ... dropped=0` while datagrams flow.

### Options

| Flag | Default | Meaning |
|---|---|---|
| `--relay <URL>` | `https://iroh-relay.distopik.com` | The self-hosted iroh relay: punch rendezvous + fallback path |
| `--peer <NodeId>` | *(host mode)* | Remote NodeId to dial |
| `--tick-hz <N>` | `60` | State datagram send rate (P0 stress) |
| `--payload-bytes <N>` | `64` | Datagram payload size |
| `--duration-secs <N>` | `30` | Test window |
| `--print-id` | — | Print NodeId and exit (host helper) |

## Reading the results

- **`path state changed path=direct`** — the pair punched through; traffic is
  direct. This is the ~90% case.
- **Stays on `relay`** — the pair is in the ~10% hard-NAT/CGNAT/UDP-blocked
  tail; the session continues over the relay (a product requirement, not an
  error — [docs/02-networking.md](../docs/02-networking.md) §8).
- **`dropped=0`** — no datagram loss at 60 Hz over the test window.

For the full P0 matrix, run pairs across ≥4 distinct NAT types and ≥2 ISPs,
including one deliberately UDP-blocked network (forced-relay case), per
[docs/11-roadmap.md](../docs/11-roadmap.md) §P0.

## Notes

- The relay is the self-hosted Hetzner box `iroh-relay.distopik.com`
  (see [.agents/memory/hetzner-relay.md](../.agents/memory/hetzner-relay.md)).
- iroh 1.0.3 API note: the endpoint must advertise the ALPN
  (`b"p0-nat-test"`) via `.alpns(...)`, and the dial must carry the relay URL
  as the addressing hint (the design's `relay_hint`) — without it the peer has
  no addressing information for the host.