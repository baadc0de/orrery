//! The gateway's witness-epoch cache and the per-cell-epoch draw key
//! ([D27](../../../docs/adr/0027-attestation-envelope.md) clause (d),
//! [D28](../../../docs/adr/0028-witness-set-seeding.md) clauses (d), (f), (g)).
//!
//! This is the state that turns an attestation from "a real signature" into
//! "a co-signature from a witness this cell's coordinator actually announced".
//! Without it the gateway has no set to check membership against and no key to
//! draw the required subset with, which is why the intent validator's own doc
//! said for two milestones that `cell_epoch` was "carried, not checked".
//!
//! # The two secrets, and why only one of them is here
//!
//! D28 clause (c)'s `k_epoch` seeds the **set selection** — which N of the
//! candidate pool are announced — and is the coordinator's alone. D27 clause
//! (d)'s `draw_key` seeds the **required-K draw** — which K of the announced N
//! must have signed *this* intent — and is generated here, held here, and
//! never sent to a peer or to the coordinator. The reason they are separate is
//! the one D27 spends its Context on: a gateway has no connection to the
//! coordinator by design, a secret cannot be couriered by the peers it is
//! meant to defend against, and the only party that consumes the required-K
//! draw is the party that checks the intent.
//!
//! # Why the gateway is allowed to hold a draw secret
//!
//! Because it grants the gateway no capability it lacks. It is already the
//! sole writer of durable truth (D11), so a compromised gateway does not need
//! to bias a draw — it can commit whatever it likes. The secret is placed with
//! the party whose compromise already ends the game rather than with the
//! parties whose compromise *is* the threat model.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use orrery_protocol::{
    attestation_draw_commitment, required_witnesses, verify_witness_epoch, CellId, GridId,
    IssuerKey, NodeId, WitnessEpochClaimsV1, WitnessEpochSnapshot, WitnessEpochVerificationError,
};

use crate::gateway::InterestAuthority;

/// One accepted announcement, plus the draw state this gateway minted for it.
///
/// Handed out behind an [`Arc`] so the intent path resolves an epoch with one
/// map lookup and then holds it without keeping the cache lock — the read
/// pattern D28 clause (f) requires ("steady state is a memory hit"), and the
/// property that keeps K-of-N enforcement off the FDB round-trip budget.
#[derive(Debug)]
pub struct AcceptedEpoch {
    /// The localized view of the announcement: the announced set, the epoch
    /// counter, and the usability window on this process's own clock.
    pub snapshot: WitnessEpochSnapshot,
    /// The coordinator-signed envelope, **verbatim**.
    ///
    /// Kept undecomposed because that is the whole security value of the
    /// durable `epoch/` row it is written into (D28 clause (f)): a later
    /// reader recomputes the coordinator signature from these bytes and needs
    /// to trust neither the gateway that wrote them nor FoundationDB. A
    /// decomposed row would be this gateway's *assertion* about an
    /// announcement.
    pub announcement: Vec<u8>,
    /// blake3 commitment to [`Self::draw_key`], published at epoch start.
    pub draw_commit: [u8; 32],
    draw_key: [u8; 32],
    committed: AtomicBool,
}

impl AcceptedEpoch {
    /// This cell-epoch's secret draw key.
    ///
    /// **Do not put this on a wire.** It has exactly two legitimate
    /// destinations: [`required_witnesses`], and the `epoch/` row inside the
    /// persistence cluster — which is not an export, because no peer holds a
    /// FoundationDB handle. D27 clause (d) makes it durable deliberately, so
    /// that a sibling gateway adopting the shard mid-epoch (D26) *reads* the
    /// key rather than minting a new one; a fresh key would silently re-roll
    /// every outstanding required subset, and every attestation already
    /// collected under the old one would stop counting.
    #[must_use]
    pub fn draw_key(&self) -> &[u8; 32] {
        &self.draw_key
    }

    /// The K announced witnesses whose co-signatures this intent must carry.
    #[must_use]
    pub fn required_witnesses(&self, intent_id: u128, eligible: &[NodeId]) -> Vec<NodeId> {
        required_witnesses(&self.draw_key, intent_id, eligible)
    }

    /// Whether this cell-epoch's `epoch/` row — and therefore its draw
    /// commitment — is known to be durable.
    ///
    /// A pure optimization, and safe to be wrong in the `false` direction: it
    /// exists so that the intent transaction stops paying for the
    /// `epoch-handle/` read once the row is known to be there. Every intent in
    /// a cell-epoch after the first therefore adds **no** FoundationDB
    /// operation at all, which is what keeps D28 clause (f)'s promise that the
    /// record costs the intent p99 nothing.
    ///
    /// Being wrong in the `true` direction is not possible: it is set only
    /// after a transaction that wrote or observed the row committed.
    #[must_use]
    pub fn is_committed(&self) -> bool {
        self.committed.load(Ordering::Relaxed)
    }

    /// Record that the `epoch/` row is durable.
    pub fn mark_committed(&self) {
        self.committed.store(true, Ordering::Relaxed);
    }
}

/// Every epoch a cell has had on file here, newest last.
#[derive(Debug, Default)]
struct CellLine {
    /// `epoch counter -> handle`, so the previous epoch's commitment is one
    /// lookup away when a reveal arrives (D28 clause (d) step 8), and so a
    /// lower-numbered epoch can be refused as a *replacement* without being
    /// discarded — intents in flight under `e` must still resolve after
    /// `e + 1` is announced (clause (g)).
    handles: BTreeMap<u32, u64>,
    /// The highest epoch counter this cell has **ever** accepted here.
    ///
    /// Separate from `handles.keys().next_back()` because it has to outlive
    /// them, and that is a replay defence rather than bookkeeping. An
    /// announcement carries only durations (D28 clause (d)), never a deadline,
    /// so an envelope signed an hour ago verifies exactly as well as one
    /// signed a second ago and this cache is the **only** thing that can
    /// refuse it. Derived from the live handles alone, step 7 would pass
    /// vacuously the moment a cell's last epoch aged out, and a peer holding
    /// interest in the cell could re-present whichever historical announced
    /// set contained the most of its colluders — accepted as fresh, with a
    /// fresh window and a newly minted draw key.
    ///
    /// (The durable `epoch/` row backstops this at commit time: a replayed
    /// epoch's row already exists under its original draw key, so the intent
    /// executor refuses on the key mismatch. This closes it at admission,
    /// which is where D10's "validate before, not after" puts it.)
    high_water: u32,
    /// When this line last accepted an announcement, on the holder's clock.
    ///
    /// The line outlives its handles, so it needs its own, much coarser,
    /// expiry — see [`CELL_LINE_RETENTION_MS`].
    last_accepted_ms: u64,
}

/// How long a cell's high-water mark is kept after its last epoch ages out.
///
/// Ten times the longest epoch a verifier will accept
/// (`MAX_WITNESS_EPOCH_MS`). The number is a trade between a replay window and
/// a map that grows with every cell this node has ever served, and it is
/// deliberately generous on the replay side: a `CellLine` is two integers and
/// an empty map, while a re-accepted stale announcement is a witness set the
/// submitter chose.
const CELL_LINE_RETENTION_MS: u64 = 10 * orrery_protocol::MAX_WITNESS_EPOCH_MS;

#[derive(Debug, Default)]
struct CacheState {
    by_handle: HashMap<u64, Arc<AcceptedEpoch>>,
    by_cell: HashMap<(GridId, CellId), CellLine>,
}

/// The gateway's verified, bounded cache of coordinator witness-set
/// announcements.
///
/// This is [`crate::gateway::CoordinatorHandoutAuthority`]'s twin: the same
/// shape (a lock over a small map, pruned on the gateway's existing 1 s
/// sweep), holding the same kind of object (a coordinator-signed claim a peer
/// couriered), verified the same way (against configured coordinator public
/// keys, with no connection to the issuer).
#[derive(Debug)]
pub struct WitnessEpochAuthority {
    keys: Vec<IssuerKey>,
    state: RwLock<CacheState>,
}

impl WitnessEpochAuthority {
    /// Build a cache that trusts exactly these coordinator keys.
    ///
    /// An empty key set is a gateway that accepts no announcements at all —
    /// every presentation answers
    /// [`WitnessEpochVerificationError::Unsupported`]. That is the honest
    /// default for a deployment that has not been given a coordinator key: it
    /// refuses to cache, rather than caching something it cannot check.
    #[must_use]
    pub fn new(keys: impl IntoIterator<Item = IssuerKey>) -> Self {
        Self {
            keys: keys.into_iter().collect(),
            state: RwLock::new(CacheState::default()),
        }
    }

    /// Verify a couriered announcement and, if it is new, accept it.
    ///
    /// This is D28 clause (d) in full: steps 1–5 are
    /// [`verify_witness_epoch`]'s (they need only the envelope and the key
    /// set), and steps 6–8 are this holder's, because they need an interest
    /// authority and a cache of accepted epochs.
    ///
    /// On acceptance a fresh 32-byte `draw_key` is drawn from the OS CSPRNG
    /// and committed to. It is minted **here**, when the announcement is
    /// accepted, and never later — D27 clause (d)'s ordering rule exists so
    /// that a gateway cannot choose `d` after seeing which attestations
    /// arrived, and minting it before any intent under the epoch can have been
    /// submitted is what enforces that.
    ///
    /// # Errors
    ///
    /// The arm of [`WitnessEpochVerificationError`] the announcement tripped.
    /// Re-presenting an announcement already on file is **not** an error: the
    /// stored one is kept (draw key included) and its epoch counter returned,
    /// because a peer re-couriering the same bytes is ordinary and re-minting
    /// a draw key for it would re-roll every outstanding required subset.
    pub fn apply_announcement(
        &self,
        encoded: &[u8],
        presenter: NodeId,
        interest: &dyn InterestAuthority,
        now_ms: u64,
    ) -> Result<u32, WitnessEpochVerificationError> {
        if self.keys.is_empty() {
            return Err(WitnessEpochVerificationError::Unsupported);
        }
        // Steps 1-5: size, decode, version, issuer, signature, pool bounds and
        // durations. Everything decidable from the bytes and the key set.
        let claims = verify_witness_epoch(encoded, &self.keys)?;

        // Step 6: the presenter must hold interest in the cell it is
        // announcing. An announcement names a cell rather than a peer, so the
        // grant path's `WrongPeer` check has no analogue — but without *some*
        // restriction any authenticated peer could stuff this cache with
        // epochs for cells it has nothing to do with. The predicate is the one
        // already on the gateway, which is D25 rule 3's seam reused rather
        // than a second notion of eligibility.
        if !interest.allows(presenter, claims.grid, claims.cell, now_ms) {
            return Err(WitnessEpochVerificationError::NotCovered);
        }

        let mut state = self.state.write().expect("witness epoch cache poisoned");

        // Step 7, first half: a handle already on file must carry the same
        // claims. The handle is globally unique by construction (D28 clause
        // (b): `(incarnation << 48) | counter`), so two different claim sets
        // under one handle is a coordinator that reused a handle or a peer
        // that edited one — either way this gateway keeps what it accepted
        // first, because intents are already being judged against it.
        if let Some(existing) = state.by_handle.get(&claims.handle) {
            if epoch_matches(&existing.snapshot, &claims) {
                return Ok(existing.snapshot.epoch);
            }
            return Err(WitnessEpochVerificationError::Superseded);
        }

        // Step 7, second half: monotonicity. A *lower* epoch is refused as a
        // replacement, not discarded — the cache is a bounded window keyed by
        // handle, not a single current value, precisely so that an intent
        // still in flight under `e` resolves after `e + 1` is announced.
        let cell = (claims.grid, claims.cell);
        //
        // Judged against the line's **high-water mark**, not against its live
        // handles: the two differ exactly once a cell's epochs have aged out,
        // and that is when a replay of an old announcement would otherwise be
        // accepted as a new one.
        //
        // `None` (no line at all) and `Some(0)` (a line whose only accepted
        // epoch was counter 0) are different answers and must stay different:
        // a cell's first epoch may legitimately be 0, and collapsing the two
        // would refuse it.
        let latest = state.by_cell.get(&cell).map(|line| line.high_water);
        if let Some(latest) = latest {
            if claims.epoch <= latest {
                return Err(WitnessEpochVerificationError::Superseded);
            }
        }

        // Step 8: the carried reveal must open the commitment this gateway
        // holds for the previous epoch, when it holds one. This is the chain
        // that makes the coordinator's reveal non-optional — it cannot issue a
        // usable `e + 1` for this cell without opening `e` — so a withheld
        // reveal costs the coordinator the cell rather than costing an auditor
        // the proof.
        let previous_handle = claims.epoch.checked_sub(1).and_then(|previous| {
            state
                .by_cell
                .get(&cell)
                .and_then(|line| line.handles.get(&previous).copied())
        });
        if let Some(previous_handle) = previous_handle {
            let previous = state
                .by_handle
                .get(&previous_handle)
                .expect("cell line names a cached handle");
            let Some(revealed) = claims.prev_seed_key else {
                return Err(WitnessEpochVerificationError::BadReveal);
            };
            if orrery_protocol::witness_epoch_commitment(
                previous.snapshot.grid,
                previous.snapshot.cell,
                previous.snapshot.epoch,
                &revealed,
            ) != previous.snapshot.seed_commitment
            {
                return Err(WitnessEpochVerificationError::BadReveal);
            }
        }

        // Accepted. Mint the draw key and commit to it before the epoch can
        // have admitted anything.
        let mut draw_key = [0u8; 32];
        getrandom::fill(&mut draw_key).expect("OS CSPRNG unavailable");
        let draw_commit =
            attestation_draw_commitment(claims.grid, claims.cell, claims.epoch, &draw_key);

        let epoch = claims.epoch;
        let handle = claims.handle;
        let accepted = Arc::new(AcceptedEpoch {
            snapshot: WitnessEpochSnapshot::from_claims(claims, now_ms),
            announcement: encoded.to_vec(),
            draw_commit,
            draw_key,
            committed: AtomicBool::new(false),
        });
        let line = state
            .by_cell
            .entry((accepted.snapshot.grid, accepted.snapshot.cell))
            .or_default();
        line.handles.insert(epoch, handle);
        line.high_water = line.high_water.max(epoch);
        line.last_accepted_ms = now_ms;
        state.by_handle.insert(handle, accepted);
        Ok(epoch)
    }

    /// The epoch an intent's `cell_epoch` handle names, if this gateway holds
    /// one.
    ///
    /// A miss is not a failure of this function: D27 clause (e) is explicit
    /// that a gateway holding no valid announcement for an intent's cell-epoch
    /// derives no required subset and admits no attestation toward K. What the
    /// caller does with a miss is the caller's rule, not the cache's.
    #[must_use]
    pub fn resolve(&self, handle: u64) -> Option<Arc<AcceptedEpoch>> {
        self.state
            .read()
            .expect("witness epoch cache poisoned")
            .by_handle
            .get(&handle)
            .map(Arc::clone)
    }

    /// Drop every epoch whose usability window has closed.
    ///
    /// Called from the gateway's existing 1 s sweep, beside
    /// [`InterestAuthority::prune_expired`]. The window is
    /// `epoch_ms + accept_grace_ms` from acceptance (D28 clause (g)), so an
    /// epoch survives its own end by the reconnect grace and is only then
    /// forgotten — which is what makes a late attestation from a netsplit
    /// survivor land as a *stale epoch* rather than as an unknown one for the
    /// grace's duration.
    pub fn prune_expired(&self, now_ms: u64) {
        let mut state = self.state.write().expect("witness epoch cache poisoned");
        let mut dropped: Vec<(GridId, CellId, u32)> = Vec::new();
        state.by_handle.retain(|_, epoch| {
            let live = now_ms < epoch.snapshot.usable_until_ms;
            if !live {
                dropped.push((
                    epoch.snapshot.grid,
                    epoch.snapshot.cell,
                    epoch.snapshot.epoch,
                ));
            }
            live
        });
        for (grid, cell, epoch) in dropped {
            if let Some(line) = state.by_cell.get_mut(&(grid, cell)) {
                line.handles.remove(&epoch);
            }
        }
        // A cell whose epochs have all aged out keeps its line — and only its
        // line — for `CELL_LINE_RETENTION_MS`, because the high-water mark in
        // it is what refuses a replayed announcement. Past that horizon the
        // line goes, or this map would grow with every cell the node has ever
        // served, which is the leak `prune_expired` exists to prevent on the
        // grant path too.
        state.by_cell.retain(|_, line| {
            !line.handles.is_empty()
                || now_ms < line.last_accepted_ms.saturating_add(CELL_LINE_RETENTION_MS)
        });
    }

    /// Replace a cached epoch's draw key with the one already durable for it.
    ///
    /// The D26 case D27 clause (d) names: a sibling gateway adopted this shard
    /// mid-epoch and finds an `epoch/` row a *previous* owner wrote, carrying
    /// that owner's draw key. The durable key is the authority — every
    /// co-signature outstanding in this cell-epoch was solicited under it, and
    /// the commitment that binds it is already published — so the cache
    /// adopts it rather than the row being overwritten. Minting a fresh key
    /// instead would silently re-roll every outstanding required subset.
    ///
    /// The intent that discovered the mismatch is **not** rescued by this: its
    /// required subset was already drawn under the wrong key at admission, so
    /// it is refused and its resubmission is judged correctly. Adopting here
    /// is what makes that resubmission, and every later intent, right.
    ///
    /// **The adopted entry is deliberately *not* marked committed.** Every
    /// intent already admitted under the stale key — concurrently in flight,
    /// or queued a moment earlier — would otherwise resolve this entry, find
    /// its key matching the durable row, and commit on a required subset
    /// drawn under a key that was never this epoch's. Leaving the flag clear
    /// keeps those intents on the path that re-derives the subset from the
    /// durable key and refuses them.
    ///
    /// Returns `false` when the handle is no longer cached — an epoch that
    /// aged out between admission and commit, which is ordinary and needs no
    /// repair.
    pub fn adopt_draw_key(&self, handle: u64, durable: [u8; 32], draw_commit: [u8; 32]) -> bool {
        let mut state = self.state.write().expect("witness epoch cache poisoned");
        let Some(existing) = state.by_handle.get(&handle) else {
            return false;
        };
        if existing.draw_key == durable {
            existing.mark_committed();
            return true;
        }
        let adopted = Arc::new(AcceptedEpoch {
            snapshot: existing.snapshot.clone(),
            announcement: existing.announcement.clone(),
            draw_commit,
            draw_key: durable,
            committed: AtomicBool::new(false),
        });
        state.by_handle.insert(handle, adopted);
        true
    }

    /// How many epochs are cached. Test and telemetry only.
    #[must_use]
    pub fn len(&self) -> usize {
        self.state
            .read()
            .expect("witness epoch cache poisoned")
            .by_handle
            .len()
    }

    /// Whether the cache holds no epoch at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Whether a re-presented announcement is the one already on file.
///
/// Compared on the fields the snapshot keeps rather than on the raw bytes: a
/// coordinator may legitimately re-sign identical claims (a rotated key), and
/// the question this answers is "is this the same epoch" and not "are these
/// the same bytes".
fn epoch_matches(snapshot: &WitnessEpochSnapshot, claims: &WitnessEpochClaimsV1) -> bool {
    snapshot.grid == claims.grid
        && snapshot.cell == claims.cell
        && snapshot.epoch == claims.epoch
        && snapshot.selected == claims.selected
        && snapshot.seed_commitment == claims.seed_commitment
}

/// Fixtures shared by this module's tests and the intent validator's.
///
/// Announcements are the one thing a K-of-N test cannot fake: the gateway
/// checks a coordinator signature over them, so every test that wants an
/// enforced epoch needs a signer. Building that in one place keeps the
/// validator's tests testing admission rather than re-deriving an envelope.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use orrery_protocol::{IssuerKeyId, WitnessEpochV1, WITNESS_SET_TARGET_N};

    /// A deterministic secret key.
    pub(crate) fn secret(seed: u8) -> iroh_base::SecretKey {
        iroh_base::SecretKey::from_bytes(&[seed; 32])
    }

    /// The coordinator every fixture announcement is signed by.
    pub(crate) fn coordinator() -> iroh_base::SecretKey {
        secret(200)
    }

    /// The issuer key this gateway trusts the fixture coordinator under.
    pub(crate) fn coordinator_keys() -> Vec<IssuerKey> {
        vec![IssuerKey::new(IssuerKeyId::new(1), coordinator().public())]
    }

    /// The secret half of witness `index`, seeded well clear of the issuer and
    /// coordinator seeds so no fixture accidentally makes a witness a party.
    ///
    /// Tests that only need identities call [`witnesses`]; tests that have to
    /// *co-sign* need the private half, and both must agree on the mapping or
    /// a fixture's announced set and its signers would silently diverge.
    pub(crate) fn witness_secret(index: u8) -> iroh_base::SecretKey {
        secret(100 + index)
    }

    /// `count` distinct witness identities.
    pub(crate) fn witnesses(count: u8) -> Vec<NodeId> {
        (0..count).map(|i| witness_secret(i).public()).collect()
    }

    /// A signed announcement naming `selected` for `(grid, cell, epoch)`.
    ///
    /// The candidate pool is `selected` in ascending order, which is the
    /// minimum a verifier accepts: the pool is checked for sortedness and for
    /// containing the selection, and nothing here is testing the coordinator's
    /// shuffle.
    pub(crate) fn announcement(
        grid: GridId,
        cell: CellId,
        epoch: u32,
        handle: u64,
        selected: &[NodeId],
        seed_key: &[u8; 32],
        prev_seed_key: Option<[u8; 32]>,
    ) -> Vec<u8> {
        assert!(selected.len() <= WITNESS_SET_TARGET_N);
        let mut candidates = selected.to_vec();
        candidates.sort_by_key(|node| *node.as_bytes());
        let claims = WitnessEpochClaimsV1::new(
            grid,
            cell,
            epoch,
            handle,
            30_000,
            30_000,
            candidates,
            selected.to_vec(),
            orrery_protocol::witness_epoch_commitment(grid, cell, epoch, seed_key),
            prev_seed_key,
            IssuerKeyId::new(1),
        );
        WitnessEpochV1::sign(claims, &coordinator())
            .expect("claims encode")
            .encode()
            .expect("envelope encodes")
    }

    /// An [`InterestAuthority`] that covers every peer for every cell.
    ///
    /// Step 6 is not what these tests are about, and a fixture that denied
    /// coverage would make every one of them fail for the wrong reason. The
    /// step is tested on its own, against this type's opposite.
    #[derive(Debug, Default)]
    pub(crate) struct CoverAllInterest;

    impl InterestAuthority for CoverAllInterest {
        fn snapshot_for(
            &self,
            _peer: NodeId,
        ) -> Option<orrery_protocol::CoordinatorInterestSnapshot> {
            None
        }

        fn allows(&self, _peer: NodeId, _grid: GridId, _cell: CellId, _now_ms: u64) -> bool {
            true
        }
    }

    /// An [`InterestAuthority`] that covers nobody.
    #[derive(Debug, Default)]
    pub(crate) struct CoverNothingInterest;

    impl InterestAuthority for CoverNothingInterest {
        fn snapshot_for(
            &self,
            _peer: NodeId,
        ) -> Option<orrery_protocol::CoordinatorInterestSnapshot> {
            None
        }

        fn allows(&self, _peer: NodeId, _grid: GridId, _cell: CellId, _now_ms: u64) -> bool {
            false
        }
    }

    /// A cache holding one accepted epoch over `selected`, accepted at
    /// `now_ms`.
    pub(crate) fn cache_with(
        handle: u64,
        selected: &[NodeId],
        now_ms: u64,
    ) -> Arc<WitnessEpochAuthority> {
        let epochs = Arc::new(WitnessEpochAuthority::new(coordinator_keys()));
        let encoded = announcement(
            GridId::ROOT,
            CellId::ROOT,
            1,
            handle,
            selected,
            &[7u8; 32],
            None,
        );
        epochs
            .apply_announcement(&encoded, secret(1).public(), &CoverAllInterest, now_ms)
            .expect("the fixture announcement is accepted");
        epochs
    }

    /// Accept one more epoch, for a **different cell**, into an existing
    /// cache.
    ///
    /// D30's tests need two cells in one cache, because that is the shape the
    /// standing predicate exists for: the cache resolves by handle and holds
    /// whatever every peer couriered, so a second cell's announced set is one
    /// `u64` away from any submitter that can reach this gateway.
    pub(crate) fn add_cell_epoch(
        epochs: &WitnessEpochAuthority,
        cell: CellId,
        handle: u64,
        selected: &[NodeId],
        now_ms: u64,
    ) {
        let encoded = announcement(GridId::ROOT, cell, 1, handle, selected, &[9u8; 32], None);
        epochs
            .apply_announcement(&encoded, secret(1).public(), &CoverAllInterest, now_ms)
            .expect("the fixture announcement is accepted");
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use orrery_protocol::{IssuerKeyId, WITNESS_SET_TARGET_N};

    #[test]
    fn an_announcement_is_accepted_once_and_keeps_its_draw_key() {
        let epochs = WitnessEpochAuthority::new(coordinator_keys());
        let selected = witnesses(WITNESS_SET_TARGET_N as u8);
        let encoded = announcement(
            GridId::ROOT,
            CellId::ROOT,
            1,
            0x0001_0000_0000_0001,
            &selected,
            &[7u8; 32],
            None,
        );

        assert_eq!(
            epochs
                .apply_announcement(&encoded, secret(1).public(), &CoverAllInterest, 1_000)
                .expect("accepted"),
            1
        );
        let first = epochs.resolve(0x0001_0000_0000_0001).expect("cached");
        assert_eq!(first.snapshot.selected, selected);
        assert_eq!(
            first.draw_commit,
            orrery_protocol::attestation_draw_commitment(
                GridId::ROOT,
                CellId::ROOT,
                1,
                first.draw_key()
            ),
            "the published commitment must open under the key that is kept"
        );

        // Re-couriering is ordinary — every peer in the cell holds the
        // announcement and any of them may present it — and it must not re-mint
        // the draw key, or every required subset outstanding under this epoch
        // would silently re-roll.
        assert_eq!(
            epochs
                .apply_announcement(&encoded, secret(2).public(), &CoverAllInterest, 2_000)
                .expect("re-presentation is accepted"),
            1
        );
        let again = epochs.resolve(0x0001_0000_0000_0001).expect("still cached");
        assert_eq!(again.draw_key(), first.draw_key());
        assert_eq!(
            again.snapshot.first_seen_ms, 1_000,
            "the window is measured from the first acceptance, not the latest"
        );
        assert_eq!(epochs.len(), 1);
    }

    #[test]
    fn a_gateway_with_no_coordinator_key_caches_nothing() {
        let epochs = WitnessEpochAuthority::new([]);
        let encoded = announcement(
            GridId::ROOT,
            CellId::ROOT,
            1,
            1,
            &witnesses(5),
            &[7u8; 32],
            None,
        );
        assert_eq!(
            epochs.apply_announcement(&encoded, secret(1).public(), &CoverAllInterest, 0),
            Err(WitnessEpochVerificationError::Unsupported),
            "refusing to cache is the honest answer to something it cannot check"
        );
        assert!(epochs.is_empty());
    }

    #[test]
    fn an_untrusted_signature_is_refused_before_anything_is_cached() {
        let epochs = WitnessEpochAuthority::new(vec![IssuerKey::new(
            IssuerKeyId::new(1),
            secret(201).public(),
        )]);
        let encoded = announcement(
            GridId::ROOT,
            CellId::ROOT,
            1,
            1,
            &witnesses(5),
            &[7u8; 32],
            None,
        );
        assert_eq!(
            epochs.apply_announcement(&encoded, secret(1).public(), &CoverAllInterest, 0),
            Err(WitnessEpochVerificationError::BadSignature)
        );
        assert!(epochs.is_empty());
    }

    #[test]
    fn step_six_refuses_a_presenter_with_no_interest_in_the_announced_cell() {
        let epochs = WitnessEpochAuthority::new(coordinator_keys());
        let encoded = announcement(
            GridId::ROOT,
            CellId::ROOT,
            1,
            1,
            &witnesses(5),
            &[7u8; 32],
            None,
        );
        assert_eq!(
            epochs.apply_announcement(&encoded, secret(1).public(), &CoverNothingInterest, 0),
            Err(WitnessEpochVerificationError::NotCovered),
            "an unrestricted presenter could stuff this cache with epochs for \
             cells it has nothing to do with"
        );
        assert!(epochs.is_empty());
    }

    #[test]
    fn step_seven_refuses_a_lower_epoch_as_a_replacement_and_keeps_the_older_one() {
        let epochs = WitnessEpochAuthority::new(coordinator_keys());
        let selected = witnesses(WITNESS_SET_TARGET_N as u8);
        let seed_two = [8u8; 32];
        let two = announcement(
            GridId::ROOT,
            CellId::ROOT,
            2,
            0x0001_0000_0000_0002,
            &selected,
            &seed_two,
            None,
        );
        epochs
            .apply_announcement(&two, secret(1).public(), &CoverAllInterest, 1_000)
            .expect("accepted");

        let one = announcement(
            GridId::ROOT,
            CellId::ROOT,
            1,
            0x0001_0000_0000_0001,
            &selected,
            &[7u8; 32],
            None,
        );
        assert_eq!(
            epochs.apply_announcement(&one, secret(1).public(), &CoverAllInterest, 1_100),
            Err(WitnessEpochVerificationError::Superseded)
        );
        assert!(
            epochs.resolve(0x0001_0000_0000_0002).is_some(),
            "refusing the replacement must not disturb what is on file"
        );
        assert_eq!(epochs.len(), 1);
    }

    #[test]
    fn step_eight_requires_the_next_announcement_to_open_the_previous_commitment() {
        let epochs = WitnessEpochAuthority::new(coordinator_keys());
        let selected = witnesses(WITNESS_SET_TARGET_N as u8);
        let seed_one = [7u8; 32];
        let one = announcement(
            GridId::ROOT,
            CellId::ROOT,
            1,
            0x0001_0000_0000_0001,
            &selected,
            &seed_one,
            None,
        );
        epochs
            .apply_announcement(&one, secret(1).public(), &CoverAllInterest, 1_000)
            .expect("accepted");

        // A coordinator that withholds the reveal cannot issue a usable next
        // epoch for the cell: the chain is what makes the reveal non-optional.
        let withheld = announcement(
            GridId::ROOT,
            CellId::ROOT,
            2,
            0x0001_0000_0000_0002,
            &selected,
            &[8u8; 32],
            None,
        );
        assert_eq!(
            epochs.apply_announcement(&withheld, secret(1).public(), &CoverAllInterest, 2_000),
            Err(WitnessEpochVerificationError::BadReveal)
        );

        let wrong = announcement(
            GridId::ROOT,
            CellId::ROOT,
            2,
            0x0001_0000_0000_0002,
            &selected,
            &[8u8; 32],
            Some([9u8; 32]),
        );
        assert_eq!(
            epochs.apply_announcement(&wrong, secret(1).public(), &CoverAllInterest, 2_000),
            Err(WitnessEpochVerificationError::BadReveal),
            "a key that does not open the commitment is not a reveal"
        );

        let honest = announcement(
            GridId::ROOT,
            CellId::ROOT,
            2,
            0x0001_0000_0000_0002,
            &selected,
            &[8u8; 32],
            Some(seed_one),
        );
        assert_eq!(
            epochs
                .apply_announcement(&honest, secret(1).public(), &CoverAllInterest, 2_000)
                .expect("the chained reveal is accepted"),
            2
        );

        // Both epochs stay resolvable across the turnover. This is D28 clause
        // (g)'s whole point: an attestation collected under epoch 1 is judged
        // against epoch 1's announced set, however many epochs have rolled
        // since.
        assert!(epochs.resolve(0x0001_0000_0000_0001).is_some());
        assert!(epochs.resolve(0x0001_0000_0000_0002).is_some());
    }

    #[test]
    fn an_epoch_is_usable_for_its_length_plus_the_grace_and_is_then_pruned() {
        let epochs = cache_with(1, &witnesses(WITNESS_SET_TARGET_N as u8), 1_000);
        let epoch = epochs.resolve(1).expect("cached");

        // `epoch_ms` (30 s) + `accept_grace_ms` (30 s) from acceptance. The
        // grace is docs/07 §7's reconnect window expressed as a duration on
        // the envelope rather than as a rule somebody has to remember.
        assert!(epoch.snapshot.usable_at(1_000));
        assert!(epoch.snapshot.usable_at(60_999));
        assert!(!epoch.snapshot.usable_at(61_000));

        epochs.prune_expired(60_999);
        assert_eq!(epochs.len(), 1, "a usable epoch survives the sweep");
        epochs.prune_expired(61_000);
        assert!(
            epochs.is_empty(),
            "and an unusable one is forgotten, or a long-lived gateway \
             accumulates one entry per cell-epoch it was ever couriered"
        );
        assert!(epochs.resolve(1).is_none());
    }

    #[test]
    fn a_replayed_announcement_is_refused_after_its_epoch_has_aged_out() {
        // An announcement carries durations, never deadlines (D28 clause (d)),
        // so an envelope signed an hour ago verifies exactly as well as one
        // signed a second ago. This cache is the only thing that can refuse
        // it, and a high-water mark derived from the *live* epochs alone would
        // stop refusing the moment the cell's last epoch aged out.
        let epochs = WitnessEpochAuthority::new(coordinator_keys());
        let selected = witnesses(WITNESS_SET_TARGET_N as u8);
        let encoded = announcement(
            GridId::ROOT,
            CellId::ROOT,
            3,
            0x0001_0000_0000_0003,
            &selected,
            &[7u8; 32],
            None,
        );
        epochs
            .apply_announcement(&encoded, secret(1).public(), &CoverAllInterest, 1_000)
            .expect("accepted");

        // Past `epoch_ms + accept_grace_ms`: the handle is forgotten.
        epochs.prune_expired(61_000);
        assert!(epochs.resolve(0x0001_0000_0000_0003).is_none());
        assert!(epochs.is_empty());

        assert_eq!(
            epochs.apply_announcement(&encoded, secret(1).public(), &CoverAllInterest, 61_000),
            Err(WitnessEpochVerificationError::Superseded),
            "the same bytes must not be re-accepted as a fresh epoch with a \
             fresh window and a newly minted draw key"
        );
        assert!(epochs.resolve(0x0001_0000_0000_0003).is_none());

        // A genuinely newer epoch for the same cell still lands, so the
        // high-water mark is a monotonicity rule and not a tombstone.
        let next = announcement(
            GridId::ROOT,
            CellId::ROOT,
            4,
            0x0001_0000_0000_0004,
            &selected,
            &[8u8; 32],
            None,
        );
        assert_eq!(
            epochs
                .apply_announcement(&next, secret(1).public(), &CoverAllInterest, 61_000)
                .expect("a newer epoch is accepted"),
            4
        );
    }

    #[test]
    fn a_cells_high_water_mark_is_eventually_forgotten() {
        // The line outlives its handles on purpose, but not forever, or this
        // map would grow with every cell the node has ever served.
        let epochs = cache_with(1, &witnesses(WITNESS_SET_TARGET_N as u8), 1_000);
        epochs.prune_expired(61_000);
        assert!(epochs.is_empty(), "the handle is gone");

        let horizon = 1_000 + CELL_LINE_RETENTION_MS;
        epochs.prune_expired(horizon - 1);
        let stale = announcement(
            GridId::ROOT,
            CellId::ROOT,
            1,
            0x0001_0000_0000_0001,
            &witnesses(WITNESS_SET_TARGET_N as u8),
            &[7u8; 32],
            None,
        );
        assert_eq!(
            epochs.apply_announcement(&stale, secret(1).public(), &CoverAllInterest, horizon - 1),
            Err(WitnessEpochVerificationError::Superseded),
            "inside the retention horizon the mark still refuses the replay"
        );

        epochs.prune_expired(horizon);
        assert_eq!(
            epochs
                .apply_announcement(&stale, secret(1).public(), &CoverAllInterest, horizon)
                .expect("past the horizon the cell is a stranger again"),
            1,
            "the bound is stated rather than infinite, and this is where it is"
        );
    }

    #[test]
    fn adopting_a_durable_draw_key_replaces_the_minted_one_and_reopens_the_check() {
        let epochs = cache_with(1, &witnesses(WITNESS_SET_TARGET_N as u8), 1_000);
        let minted = *epochs.resolve(1).expect("cached").draw_key();
        assert!(!epochs.resolve(1).expect("cached").is_committed());

        // The D26 handover: a sibling owned this shard when the epoch was
        // accepted, and its key is the one every outstanding co-signature was
        // solicited under.
        let durable = [42u8; 32];
        assert!(epochs.adopt_draw_key(1, durable, [43u8; 32]));
        let adopted = epochs.resolve(1).expect("cached");
        assert_eq!(adopted.draw_key(), &durable);
        assert_ne!(adopted.draw_key(), &minted);
        assert_eq!(adopted.draw_commit, [43u8; 32]);
        assert!(
            !adopted.is_committed(),
            "adoption must NOT re-open the fast path: intents already admitted \
             under the stale key are still in flight, and the durable \
             re-derivation is the only thing that refuses them"
        );
        assert_eq!(
            adopted.snapshot.first_seen_ms, 1_000,
            "adoption changes the key, never the window the epoch was accepted in"
        );

        assert!(
            !epochs.adopt_draw_key(9, durable, [43u8; 32]),
            "an epoch that aged out between admission and commit needs no repair"
        );
    }
}
