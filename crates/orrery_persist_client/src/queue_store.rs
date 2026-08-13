//! Disk-backed offline intent queue (netsplit posture, D12).
//!
//! While the gateway is unreachable, intents queue locally. With the
//! `disk-queue` feature they are appended to a fjall-backed store so they
//! survive process exit and replay on the next run — idempotency keys make
//! replay safe (D11 §2.2). Without the feature the queue is in-memory only.

use std::path::Path;

use orrery_protocol::Intent;

/// A durable store for queued intents.
///
/// The default in-memory store is used when the `disk-queue` feature is off or
/// no `queue_dir` is configured. The fjall-backed store (feature `disk-queue`)
/// appends each intent as a postcard record keyed by its idempotency key.
pub trait QueueStore: Send + Sync + std::fmt::Debug {
    /// Append an intent to the store.
    fn push(&mut self, intent: &Intent) -> Result<(), String>;
    /// Load all stored intents, in append order.
    fn load(&self) -> Vec<Intent>;
    /// Remove a committed/rejected intent from the store.
    fn remove(&mut self, intent_id: u128) -> Result<(), String>;
}

/// An in-memory queue store (no persistence).
#[derive(Debug, Default)]
pub struct MemQueueStore {
    intents: Vec<Intent>,
}

impl MemQueueStore {
    /// A new, empty in-memory store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl QueueStore for MemQueueStore {
    fn push(&mut self, intent: &Intent) -> Result<(), String> {
        self.intents.push(intent.clone());
        Ok(())
    }

    fn load(&self) -> Vec<Intent> {
        self.intents.clone()
    }

    fn remove(&mut self, intent_id: u128) -> Result<(), String> {
        self.intents.retain(|i| i.intent_id != intent_id);
        Ok(())
    }
}

#[cfg(feature = "disk-queue")]
mod disk {
    use super::*;
    use fjall::{Database, Keyspace, KeyspaceCreateOptions};

    /// The fjall keyspace holding queued intents.
    const INTENTS_KS: &str = "intents";

    /// A fjall-backed queue store.
    ///
    /// Intents are appended as postcard records keyed by their idempotency key
    /// (`intent_id`), so a crash mid-queue leaves the remaining intents intact
    /// and replay is safe (idempotency keys, D11 §2.2).
    pub struct DiskQueueStore {
        _db: Database,
        intents: Keyspace,
    }

    impl std::fmt::Debug for DiskQueueStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("DiskQueueStore")
                .field("intents", &self.intents.len())
                .finish()
        }
    }

    impl DiskQueueStore {
        /// Open (or create) the store at `dir`.
        ///
        /// # Errors
        ///
        /// Returns an error if the store cannot be opened.
        pub fn open(dir: &Path) -> Result<Self, String> {
            let db = Database::builder(dir)
                .open()
                .map_err(|e| format!("open queue store: {e}"))?;
            let intents = db
                .keyspace(INTENTS_KS, KeyspaceCreateOptions::default)
                .map_err(|e| format!("open intents keyspace: {e}"))?;
            Ok(Self { _db: db, intents })
        }
    }

    impl QueueStore for DiskQueueStore {
        fn push(&mut self, intent: &Intent) -> Result<(), String> {
            let key = intent.intent_id.to_be_bytes();
            let value = postcard::to_stdvec(intent).map_err(|e| format!("encode: {e}"))?;
            self.intents
                .insert(key, value)
                .map_err(|e| format!("insert: {e}"))?;
            Ok(())
        }

        fn load(&self) -> Vec<Intent> {
            let mut out = Vec::new();
            for item in self.intents.iter() {
                let Ok((_key, value)) = item.into_inner() else {
                    continue;
                };
                if let Ok(intent) = postcard::from_bytes(value.as_ref()) {
                    out.push(intent);
                }
            }
            out
        }

        fn remove(&mut self, intent_id: u128) -> Result<(), String> {
            self.intents
                .remove(intent_id.to_be_bytes())
                .map_err(|e| format!("remove: {e}"))?;
            Ok(())
        }
    }
}

/// Open a queue store from the config.
///
/// Returns a disk-backed store when the `disk-queue` feature is on and
/// `queue_dir` is set, otherwise an in-memory store.
#[must_use]
#[allow(unused_variables)]
pub fn open_store(queue_dir: Option<&Path>) -> Box<dyn QueueStore> {
    #[cfg(feature = "disk-queue")]
    if let Some(dir) = queue_dir {
        if let Ok(store) = disk::DiskQueueStore::open(dir) {
            return Box::new(store);
        }
    }
    Box::new(MemQueueStore::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::{Attestation, Epoch, IntentOp};

    fn node(n: u8) -> orrery_protocol::NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh_base::SecretKey::from_bytes(&seed).public()
    }

    fn sig() -> orrery_protocol::Signature {
        let seed = [0u8; 32];
        iroh_base::SecretKey::from_bytes(&seed).sign(b"test")
    }

    fn intent(id: u128) -> Intent {
        Intent {
            intent_id: id,
            issuer: node(1),
            cell_epoch: Epoch::new(0),
            ops: vec![IntentOp {
                op: 1,
                args: bytes::Bytes::from_static(b"trade"),
            }],
            attestations: vec![Attestation {
                witness: node(2),
                signature: sig(),
            }],
            signature: sig(),
        }
    }

    #[test]
    fn mem_store_roundtrips() {
        let mut store = MemQueueStore::new();
        store.push(&intent(1)).unwrap();
        store.push(&intent(2)).unwrap();
        assert_eq!(store.load().len(), 2);
        store.remove(1).unwrap();
        assert_eq!(store.load().len(), 1);
    }

    #[cfg(feature = "disk-queue")]
    #[test]
    fn disk_store_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = disk::DiskQueueStore::open(dir.path()).unwrap();
        store.push(&intent(1)).unwrap();
        store.push(&intent(2)).unwrap();
        assert_eq!(store.load().len(), 2);
        store.remove(1).unwrap();
        assert_eq!(store.load().len(), 1);
    }
}
