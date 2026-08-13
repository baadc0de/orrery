//! Disk-backed offline intent queue (netsplit posture, D12).
//!
//! While the gateway is unreachable, intents queue locally. With the
//! `disk-queue` feature they are appended to a fjall-backed store so they
//! survive process exit and replay on the next run — idempotency keys make
//! replay safe (D11 §2.2). Without the feature the queue is in-memory only.
//!
//! Keys are monotonic submission sequences rather than `intent_id`, so
//! [`load`](QueueStore::load) returns intents in submission order (the
//! ordering the doc contract at [`QueueStore::load`] promises).

use std::path::Path;

use orrery_protocol::Intent;

/// A durable store for queued intents.
///
/// The default in-memory store is used when the `disk-queue` feature is off or
/// no `queue_dir` is configured. The fjall-backed store (feature `disk-queue`)
/// appends each intent as a postcard record keyed by a monotonic submission
/// sequence, so [`load`](Self::load) returns intents in submission order.
pub trait QueueStore: Send + Sync + std::fmt::Debug {
    /// Append an intent to the store.
    fn push(&mut self, intent: &Intent) -> Result<(), String>;
    /// Load all stored intents, **in submission order**.
    fn load(&self) -> Vec<Intent>;
    /// Remove a committed/rejected intent from the store.
    fn remove(&mut self, intent_id: u128) -> Result<(), String>;
}

/// An in-memory queue store (no persistence).
///
/// Intents are appended in submission order via a monotonic sequence counter,
/// so [`load`](QueueStore::load) returns them in submission order.
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
    /// Intents are appended as postcard records keyed by a monotonic submission
    /// sequence (a big-endian u64 counter), so [`load`](QueueStore::load) returns
    /// them in submission order. The sequence counter is stored in a separate
    /// key `b"seq"` in the same keyspace.
    pub struct DiskQueueStore {
        _db: Database,
        intents: Keyspace,
        /// The next submission sequence number.
        next_seq: u64,
    }

    impl std::fmt::Debug for DiskQueueStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("DiskQueueStore")
                .field("next_seq", &self.next_seq)
                .field("intents", &self.intents.len())
                .finish()
        }
    }

    impl DiskQueueStore {
        /// The key for the sequence counter.
        const SEQ_KEY: &'static [u8] = b"seq";

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
            // Read the last sequence from the store, or start at 0.
            let next_seq = intents
                .get(Self::SEQ_KEY)
                .ok()
                .flatten()
                .map(|v| {
                    let arr: [u8; 8] = v.as_ref().try_into().unwrap_or([0; 8]);
                    u64::from_be_bytes(arr)
                })
                .unwrap_or(0);
            Ok(Self {
                _db: db,
                intents,
                next_seq,
            })
        }
    }

    impl QueueStore for DiskQueueStore {
        fn push(&mut self, intent: &Intent) -> Result<(), String> {
            let seq = self.next_seq;
            let key = seq.to_be_bytes();
            let value = postcard::to_stdvec(intent).map_err(|e| format!("encode: {e}"))?;
            // Write the sequence key as a marker for ordering.
            self.intents
                .insert(key, value)
                .map_err(|e| format!("insert: {e}"))?;
            // Persist the seq counter so the next open continues from here.
            self.intents
                .insert(Self::SEQ_KEY.to_vec(), seq.to_be_bytes().to_vec())
                .map_err(|e| format!("persist seq: {e}"))?;
            self.next_seq += 1;
            Ok(())
        }

        fn load(&self) -> Vec<Intent> {
            let mut out: Vec<(u64, Intent)> = Vec::new();
            for item in self.intents.iter() {
                let Ok((key, value)) = item.into_inner() else {
                    continue;
                };
                // Skip the seq marker key.
                if key.as_ref() == Self::SEQ_KEY {
                    continue;
                }
                let seq = {
                    let arr: [u8; 8] = key.as_ref().try_into().unwrap_or([0; 8]);
                    u64::from_be_bytes(arr)
                };
                if let Ok(intent) = postcard::from_bytes(value.as_ref()) {
                    out.push((seq, intent));
                }
            }
            // Sort by submission sequence to guarantee order.
            out.sort_by_key(|(seq, _)| *seq);
            out.into_iter().map(|(_, intent)| intent).collect()
        }

        fn remove(&mut self, intent_id: u128) -> Result<(), String> {
            // We need to find the key for this intent_id. Iterate to find it.
            let mut to_remove: Option<Vec<u8>> = None;
            for item in self.intents.iter() {
                let Ok((key, value)) = item.into_inner() else {
                    continue;
                };
                if key.as_ref() == Self::SEQ_KEY {
                    continue;
                }
                if let Ok(intent) = postcard::from_bytes::<Intent>(value.as_ref()) {
                    if intent.intent_id == intent_id {
                        to_remove = Some(key.as_ref().to_vec());
                        break;
                    }
                }
            }
            if let Some(key) = to_remove {
                self.intents
                    .remove(key)
                    .map_err(|e| format!("remove: {e}"))?;
            }
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

    #[test]
    fn mem_store_loads_in_submission_order() {
        let mut store = MemQueueStore::new();
        // Push with ids in descending order; load must return submission order.
        store.push(&intent(100)).unwrap();
        store.push(&intent(50)).unwrap();
        store.push(&intent(200)).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].intent_id, 100);
        assert_eq!(loaded[1].intent_id, 50);
        assert_eq!(loaded[2].intent_id, 200);
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

    #[cfg(feature = "disk-queue")]
    #[test]
    fn disk_queue_replays_in_submission_order() {
        // Write intents whose ids are deliberately in DESCENDING numeric order
        // and assert that load() returns them in submission order.
        let dir = tempfile::tempdir().unwrap();
        let mut store = disk::DiskQueueStore::open(dir.path()).unwrap();
        // Push with ids 100, 50, 200 (descending order).
        store.push(&intent(100)).unwrap();
        store.push(&intent(50)).unwrap();
        store.push(&intent(200)).unwrap();
        let loaded = store.load();
        // The loaded order must be the submission order: 100, 50, 200.
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].intent_id, 100);
        assert_eq!(loaded[1].intent_id, 50);
        assert_eq!(loaded[2].intent_id, 200);
    }

    #[cfg(feature = "disk-queue")]
    #[test]
    fn disk_store_sequence_increments_across_pushes() {
        // Verify that sequence numbers are monotonic within a single session.
        let dir = tempfile::tempdir().unwrap();
        let mut store = disk::DiskQueueStore::open(dir.path()).unwrap();
        store.push(&intent(10)).unwrap();
        store.push(&intent(20)).unwrap();
        store.push(&intent(30)).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].intent_id, 10);
        assert_eq!(loaded[1].intent_id, 20);
        assert_eq!(loaded[2].intent_id, 30);
    }
}
