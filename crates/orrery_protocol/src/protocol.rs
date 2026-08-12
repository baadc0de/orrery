//! Protocol versioning (D15).

/// The wire protocol version. Services accept `PROTOCOL_VERSION` and
/// `PROTOCOL_VERSION − 1` (rolling upgrade).
pub const PROTOCOL_VERSION: u16 = 1;
