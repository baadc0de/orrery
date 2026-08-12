# `aeronet_iroh`

> **Orrery vendored copy.** Upstream (`aecsocket/aeronet`) has not published
> `aeronet_iroh` on crates.io as of Aug 2026, so this repo carries a vendored
> copy of `crates/aeronet_iroh` @ aeronet `main`. Orrery's local diff is
> intentionally small and meant to become an upstream PR (orrery ADR D4):
>
> 1. `session::PathReport` + `session::PathKind` — per-session path telemetry:
>    relay-vs-direct, RTT, and time-to-direct-path (`direct_since` -
>    `connected_at`).
> 2. The backend meta loop now also wakes on `Connection::path_events()`
>    (`Selected`/`Closed`), so relay→direct migration is reported within a
>    frame instead of up to 100 ms late (native targets only; WASM keeps the
>    plain interval because `PathEventStream` is not fused and `tokio::select!`
>    is unavailable there).
> 3. Extra test `tests/relay_path_report.rs` (in-process loopback relay via
>    `iroh::test_utils::run_relay_server`, no external network).
>
> Everything below is the upstream README, unmodified.

[![crates.io](https://img.shields.io/crates/v/aeronet_iroh.svg)](https://crates.io/crates/aeronet_iroh)
[![docs.rs](https://docs.rs/aeronet_iroh/badge.svg)](https://docs.rs/aeronet_iroh)
[![license](https://img.shields.io/crates/l/aeronet_iroh.svg)](https://github.com/aecsocket/aeronet)

[Iroh](https://iroh.computer) IO layer implementation for `aeronet`.

Iroh establishes authenticated peer-to-peer QUIC connections. It attempts to connect peers directly, including through NAT hole punching, and can fall back to an encrypted relay path when a direct path is unavailable.

An [`IrohEndpoint`](https://docs.rs/aeronet_iroh/latest/aeronet_iroh/endpoint/struct.IrohEndpoint.html) both accepts incoming sessions and initiates outgoing sessions. There is no separate client or server role.

## Getting started

Add [`IrohPlugin`](https://docs.rs/aeronet_iroh/latest/aeronet_iroh/struct.IrohPlugin.html) to your app, then open an endpoint:

```no_run
use {
    aeronet_iroh::{IrohPlugin, endpoint::IrohEndpoint},
    bevy::prelude::*,
    iroh::endpoint::presets,
};

const ALPN: &[u8] = b"my-game/0";

fn main() {
    App::new()
        .add_plugins((MinimalPlugins, IrohPlugin))
        .add_systems(Startup, open_endpoint)
        .run();
}

fn open_endpoint(mut commands: Commands) {
    let builder = iroh::Endpoint::builder(presets::N0).alpns(vec![ALPN.to_vec()]);
    commands.spawn_empty().queue(IrohEndpoint::open(builder));
}
```

The ALPN identifies your application protocol. Configure the endpoint with the ALPNs it accepts and pass the selected ALPN to [`IrohEndpoint::connect`](https://docs.rs/aeronet_iroh/latest/aeronet_iroh/endpoint/struct.IrohEndpoint.html#method.connect) when opening an outgoing session. Aeronet does not define an ALPN of its own.

The `N0` preset uses Iroh's public address lookup and relay infrastructure. Pass an [`EndpointAddr`](https://docs.rs/iroh/latest/iroh/struct.EndpointAddr.html) to include known direct or relay addresses, or pass an [`EndpointId`](https://docs.rs/iroh/latest/iroh/type.EndpointId.html) and let the configured address lookup services resolve it.

Relay use is optional on native targets. Use Iroh's `N0DisableRelay` preset for direct connections with public address lookup, or `Minimal` for a direct-only endpoint with no relay or address lookup services. Browser endpoints require a relay because browsers cannot open direct UDP sockets.

Run the peer example to print its local endpoint ID:

```sh
cargo run --example iroh_peer
```

Then connect a second peer to that endpoint:

```sh
cargo run --example iroh_peer -- --remote ENDPOINT
```

## Compatibility

| `aeronet_iroh` | `aeronet` | `bevy` | `iroh` |
| --------------- | --------- | ------ | ------ |
| `0.21`          | `0.21`    | `0.19` | `1.0`  |

Dual-licensed under MIT or Apache 2.0.
