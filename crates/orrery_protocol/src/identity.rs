//! Transport identity and signatures (D3).
//!
//! iroh's ed25519 key is the transport identity: a peer's `NodeId` is its
//! [`iroh_base::PublicKey`] (aliased `EndpointId`). Signatures use iroh's
//! ed25519 `Signature` type. These are re-exported here so wire types can name
//! them without depending on iroh's full endpoint machinery.

pub use iroh_base::Signature;

/// A peer's transport identity — iroh's ed25519 public key (D3).
pub type NodeId = iroh_base::PublicKey;
