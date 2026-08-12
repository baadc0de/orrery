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

### Local mesh test (one machine)

To stand up a host plus several clients on one box (e.g. the P0 8-peer mesh):

```sh
# terminal 1 — host accepting 8 connections
./p0-nat-test --peers 8 --duration-secs 1800

# terminals 2..9 — each a client
./p0-nat-test --peer <host-node-id> --duration-secs 1800
```

The host logs each connection as `peer=N`, and every peer punches to a direct
path and exchanges datagrams independently.

### Options

| Flag | Default | Meaning |
|---|---|---|
| `--relay <URL>` | `https://iroh-relay.distopik.com` | The self-hosted iroh relay: punch rendezvous + fallback path |
| `--peer <NodeId>` | *(host mode)* | Remote NodeId to dial |
| `--peers <N>` | `1` | Host mode: accept N simultaneous connections (local mesh test) |
| `--tick-hz <N>` | `60` | State datagram send rate (P0 stress) |
| `--payload-bytes <N>` | `64` | Datagram payload size |
| `--duration-secs <N>` | `30` | Test window |
| `--print-id` | — | Print NodeId and exit (host helper) |
| `--json` | off | Emit telemetry as one JSON object per line on stdout (tracing → stderr) |

### Machine-readable telemetry (`--json`)

For the punch-rate dashboard, run with `--json` and capture stdout:

```sh
./p0-nat-test --json --duration-secs 1800 > host.jsonl
```

Each line is one record, e.g.:

```json
{"ts":1786532620521,"node":"c132…","role":"host","peer":0,"type":"path","path":"direct","ttd_ms":55}
{"ts":1786532625576,"node":"0c9c…","role":"peer","peer":0,"type":"stats","sent":310,"received":304,"dropped":0}
```

Record types: `connected` (remote NodeId), `path` (relay/direct/mixed +
`ttd_ms` = time-to-direct-path), `stats` (sent/received/dropped per 10s
window), `error`. Correlate host and peer sides of a pair by `node`/`remote`.
The direct-path rate and `ttd_ms` distribution are the P0 punch metrics
([docs/11-roadmap.md](../docs/11-roadmap.md) §P0).

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