//! Protocol versioning (D15).

/// The wire protocol version. Services accept **this version only**.
///
/// # Version 2, and why the rolling-upgrade window is gone
///
/// [D29](https://github.com/baadc0de/orrery/blob/main/docs/adr/0029-low-population-path.md)
/// clause 5 appends a third arm to
/// [`IntentOutcome`](crate::IntentOutcome). postcard keys enum variants by
/// declaration order, so appending an arm is safe to encode-old/decode-new and
/// unsafe in the other direction: a version-1 client handed `Provisional`
/// fails to decode it. That is the first real protocol break since this
/// constant was introduced, and the operator's decision on accepting D29 was
/// to take it with **the window closed** — the cluster supports version 2 and
/// nothing else.
///
/// The record states the reasoning and this comment repeats it because the
/// constant is where a reader looks: the system is pre-release and has no
/// external clients, so dual-version support was complexity that had not been
/// earned, and keeping the window would have meant carrying a second admission
/// branch — "a version-1 client is refused the provisional path" — through
/// every site D29 touches, to serve a client that does not exist.
///
/// **The closure is once, for all traffic, not per message family.** A
/// version-1 peer cannot decode an intent reply, so letting it negotiate at
/// all would only move the failure to the first low-population commit in the
/// cell it happens to be standing in.
///
/// The window is enforced by
/// [`GatewayMsg::protocol_accepted`](crate::GatewayMsg::protocol_accepted), and
/// only against clients that bootstrap with
/// [`GatewayMsg::VersionedHello`](crate::GatewayMsg::VersionedHello): the
/// unversioned [`GatewayMsg::Hello`](crate::GatewayMsg::Hello) is still
/// accepted unchecked, so enforcement is opt-in until that variant is removed.
pub const PROTOCOL_VERSION: u16 = 2;
