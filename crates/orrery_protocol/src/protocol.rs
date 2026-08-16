//! Protocol versioning (D15).

/// The wire protocol version. Services accept `PROTOCOL_VERSION` and
/// `PROTOCOL_VERSION − 1` (rolling upgrade), so a cluster always deploys ahead
/// of its clients.
///
/// The window is enforced by
/// [`GatewayMsg::protocol_accepted`](crate::GatewayMsg::protocol_accepted), and
/// only against clients that bootstrap with
/// [`GatewayMsg::VersionedHello`](crate::GatewayMsg::VersionedHello): the
/// unversioned [`GatewayMsg::Hello`](crate::GatewayMsg::Hello) is still
/// accepted unchecked, so enforcement is opt-in until that variant is removed.
pub const PROTOCOL_VERSION: u16 = 1;
