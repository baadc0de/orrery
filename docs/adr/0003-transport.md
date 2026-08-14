# ADR-0003: Transport: iroh 1.0 (P2P QUIC + hole punching + relays)

**Status:** Accepted · **Date:** 2026-08-11 · **Decision:** D3

This decision is normative. See the [ADR index](../DECISIONS.md) for precedence, scope, and the complete decision set.

The P2P transport is **[iroh](https://github.com/n0-computer/iroh) 1.x**: QUIC connections dialed by ed25519 public key (`NodeId`), with NAT hole punching coordinated through stateless, self-hostable relays (`iroh-relay`), ~90% direct-connection success in production (~95% of bytes on direct paths), relay fallback for the rest. Connections start over the relay immediately and migrate to the direct path via QUIC multipath (iroh ≥1.0 runs on `noq`, which implements RFC 9221 datagrams, QUIC multipath, QUIC Address Discovery, and the IETF QUIC NAT-traversal draft). One connection carries **unreliable datagrams** (state replication) and **reliable streams** (control, bulk transfer) with no head-of-line blocking between them.

- The relay fleet is **self-hosted** (`iroh-relay`: public IP + DNS + ACME; ≥3 regions at launch: 2×US/EU + Asia). Relays double as the hole-punch rendezvous; the ~5–10% permanently-relayed tail (CGNAT↔CGNAT, UDP-blocked networks) is a **product requirement**, provisioned for, not an edge case.
- iroh `NodeId`s are the **transport identity everywhere**: peer↔peer, peer↔cluster. The persistence gateway and coordinator are ordinary iroh nodes at well-known public addresses (no punching needed server-side).

**Rejected:** rust-libp2p + DCUtR (~70% measured punch success, 13-month release gap); DIY punching over quinn with STUN/ICE crates (reproduces iroh at DIY quality; still needs a relay fleet); matchbox/WebRTC as primary (native webrtc-rs is a heavy ICE+DTLS+SCTP stack; only wins if WASM parity mattered — R9 says it doesn't). **Hedge:** all networking goes through the `aeronet_io` abstraction, so a raw-quinn backend (LAN, dedicated server, tests) is a drop-in; if iroh's identity layer ever chafes, `noq` is usable standalone.

