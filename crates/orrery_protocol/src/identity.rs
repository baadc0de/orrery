//! Transport identity and signatures (D3).
//!
//! iroh's ed25519 key is the transport identity: a peer's `NodeId` is its
//! [`iroh_base::PublicKey`] (aliased `EndpointId`). Signatures use iroh's
//! ed25519 `Signature` type. These are re-exported here so wire types can name
//! them without depending on iroh's full endpoint machinery.

pub use iroh_base::Signature;

/// A peer's transport identity — iroh's ed25519 public key (D3).
pub type NodeId = iroh_base::PublicKey;

#[cfg(test)]
mod tests {
    /// Deterministic [`NodeId`] from a one-byte discriminant.
    fn node(n: u8) -> super::NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh_base::SecretKey::from_bytes(&seed).public()
    }

    fn sig() -> super::Signature {
        let seed = [0u8; 32];
        iroh_base::SecretKey::from_bytes(&seed).sign(b"test")
    }

    #[test]
    fn node_id_from_secret_key() {
        let a = node(1);
        let b = node(2);
        // Different seeds produce different public keys.
        assert_ne!(a, b);
        // The key is not the all-zeros sentinel.
        assert_ne!(a, super::NodeId::from_bytes(&[0u8; 32]).unwrap());
    }

    #[test]
    fn node_id_equality_and_clone() {
        let a = node(42);
        let b = node(42);
        assert_eq!(a, b);
        assert_eq!(a, a.clone());
    }

    #[test]
    fn node_id_from_bytes_roundtrip() {
        let a = node(7);
        let raw = a.as_bytes();
        let back = super::NodeId::from_bytes(raw).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn signature_create_and_verify() {
        let msg = b"hello orrery";
        let sk = iroh_base::SecretKey::from_bytes(&[8u8; 32]);
        let pk = sk.public();
        let signature = sk.sign(msg);
        // The signature verifies against the public key that produced it.
        assert!(pk.verify(msg, &signature).is_ok());
    }

    #[test]
    fn signature_inequality() {
        let sk = iroh_base::SecretKey::from_bytes(&[8u8; 32]);
        let sig_a = sk.sign(b"message one");
        let sig_b = sk.sign(b"message two");
        // Different messages produce different signatures.
        assert_ne!(sig_a, sig_b);
    }

    #[test]
    fn node_id_postcard_roundtrip() {
        let a = node(99);
        let bytes = postcard::to_stdvec(&a).unwrap();
        let back: super::NodeId = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn signature_postcard_roundtrip() {
        let a = sig();
        let bytes = postcard::to_stdvec(&a).unwrap();
        let back: super::Signature = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(a, back);
    }
}
