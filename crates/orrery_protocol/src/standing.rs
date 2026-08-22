//! Standing propagation to sessions that are already open (D33 clause (e)).
//!
//! # What this is for
//!
//! `SessionStanding` is a *signed* claim, and every enforcement point reads it
//! out of a token it verified once, at handshake. That is correct for
//! authenticity and wrong for latency: a quarantine filed at `t` is invisible
//! until the offender's next token — up to half a TTL if the client refreshes
//! as designed, a full hour if it does not, and *indefinitely* if it simply
//! stops refreshing while holding the connection open, because nothing
//! re-checks an established session's token.
//!
//! The reverse direction is the half that is easy to forget and just as real:
//! an account whose quarantine has **lifted** keeps paying D10's full
//! cluster-side validation cost it no longer owes, for exactly as long.
//!
//! # Quarantine is not cooldown or ban, and this is not that mechanism
//!
//! D33 clause (e) splits the standing machine along a line this module sits on
//! one side of. Cooldown and ban are **admission** decisions — identity
//! refuses to mint, and a session that outlives the decision is *terminated*,
//! which is what `orrery_persistd`'s `AccountInvalidations` and its
//! `standing_sweep` do. Quarantine is not: the session stays, and what changes
//! is what it may do (no witness eligibility under D28 clause (e); D10 full
//! cluster-side validation on the write path). So the mechanism here **updates
//! a live session's standing in place**, and must not perturb anything a
//! termination would have reset — the gateway's session generation and the
//! leases keyed by it, or the coordinator's `joined_ms`.
//!
//! # One control, one lever
//!
//! Both halves are D32's control **C5** (`strikes`), so both are gated by the
//! *same* posture cell — `orrery_persistd::gateway::StrikesPosture`, which
//! this module deliberately does not duplicate. A second cell for one control
//! would mean an operator demoting C5 under clause (f) demoted only half of
//! it, which is the failure that lever exists to prevent. Hence the shape
//! below: [`AccountStandings::pending`] answers *what* would change and counts
//! nothing, and the caller — which is the party holding the posture — decides
//! whether to apply it and records which way it went.
//!
//! # The watermark rule, and why no new token field was needed
//!
//! An [`AccountStandingUpdate`] carries the standing identity now asserts and
//! the instant from which it asserts it. It applies to a session whose token
//! was signed **before** that instant, and to no other:
//!
//! ```text
//! claims.issued_at_ms  <  update.effective_from_ms   =>  the update wins
//! claims.issued_at_ms  >= update.effective_from_ms   =>  the token wins
//! ```
//!
//! This is deliberately the same rule `AccountInvalidations` applies to the
//! termination half, read for a value instead of for a kill. A token issued at
//! or after the watermark exists only because identity answered for that
//! account again, with the ledger in front of it (D33 clause (f)), so it
//! already carries the current answer and stands on its own merits. That
//! single comparison is what makes the mechanism direction-free — applying a
//! quarantine and lifting one are the same code path — and it rides
//! `issued_at_ms`, a field token V1 already signs. **No new token field, and
//! no new `SessionStanding` variant**: clause (e) keeps that enum two-valued
//! deliberately, because cooldown and ban are not claims a connected peer has
//! to interpret.
//!
//! It also composes with docs/09 §8's grace rule in the safe direction. During
//! an identity outage the freshest available token is stale by definition, so
//! its `issued_at_ms` loses to any update published before the outage began —
//! and one published *during* the outage cannot exist, because identity is
//! what publishes.
//!
//! # What it costs on the hot path
//!
//! Nothing. [`AccountStandings`] is consulted when a session is admitted and
//! on a periodic sweep, never per intent: the resolved standing is kept
//! wherever the enforcement point already keeps its per-session facts. D16's
//! 10 ms intent-commit budget therefore gains no store access and no lock —
//! see `orrery_persistd`'s registrar, which holds the resolved value in the
//! same shape, and for the same reason, as `last_seen_ms`.

use crate::{AccountId, SessionStanding, SessionTokenClaimsV1, UnixMillis};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

/// One account's standing as identity asserts it *now*, and from when.
///
/// Not a wire message on its own: how a fleet distributes these is the
/// publisher's business (a durable row poll, a fan-out, a test seam). This is
/// the shape every consumer agrees on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountStandingUpdate {
    /// The durable account the assertion is about.
    pub account: AccountId,
    /// The standing identity asserts for it.
    pub standing: SessionStanding,
    /// The Unix millisecond instant from which the assertion holds. A token
    /// signed at or after this loses nothing to it; see the module docs.
    pub effective_from_ms: UnixMillis,
}

/// Where a consumer gets its updates.
///
/// Deliberately synchronous and infallible, unlike the *termination* half's
/// `StandingInvalidationFeed`, which is `async` and returns a `Result`. The
/// asymmetry is not an oversight and is worth stating: `orrery_protocol` links
/// no runtime (D15), both enforcement points already own a periodic tick to
/// drain on, and the only implementation in the tree is an in-process queue
/// that cannot fail. When identity's publisher lands, the two seams should be
/// unified by whichever record specifies it — carrying one publication in two
/// shapes is a wart, not a design.
pub trait StandingUpdateFeed: std::fmt::Debug + Send + Sync {
    /// Take everything published since the last call. An empty `Vec` is the
    /// ordinary answer and must be cheap.
    fn drain(&self) -> Vec<AccountStandingUpdate>;
}

/// The consumer both enforcement points hold: the latest assertion per
/// account, and the counters C5's ramp reads.
///
/// One entry per account *identity has spoken about since this process
/// started* — not per session, and not per account in the world. The map is
/// read on admission and on a periodic sweep; nothing on an intent path
/// touches it.
///
/// It holds no posture. See the module docs: the posture is C5's one cell, and
/// the caller owns it.
#[derive(Debug)]
pub struct AccountStandings {
    feed: Option<Arc<dyn StandingUpdateFeed>>,
    latest: RwLock<HashMap<AccountId, AccountStandingUpdate>>,
    applied: AtomicU64,
    observed: AtomicU64,
}

impl Default for AccountStandings {
    fn default() -> Self {
        Self::inert()
    }
}

impl AccountStandings {
    /// A consumer with no feed: it polls nothing and resolves nothing, ever.
    ///
    /// This is the shipped default at both enforcement points, so a deployment
    /// that configures no feed behaves exactly as it did before this module
    /// existed — and with C5 at `Off`, which is *its* default, not even the
    /// poll happens.
    #[must_use]
    pub fn inert() -> Self {
        Self {
            feed: None,
            latest: RwLock::new(HashMap::new()),
            applied: AtomicU64::new(0),
            observed: AtomicU64::new(0),
        }
    }

    /// A consumer reading `feed`.
    #[must_use]
    pub fn new(feed: Arc<dyn StandingUpdateFeed>) -> Self {
        Self {
            feed: Some(feed),
            latest: RwLock::new(HashMap::new()),
            applied: AtomicU64::new(0),
            observed: AtomicU64::new(0),
        }
    }

    /// Drain the feed into the map. Called from the caller's periodic tick,
    /// and only once the caller has established that C5 is not `Off` — D32
    /// clause (b): a control that does not exist observes nothing, not even a
    /// poll.
    ///
    /// Returns how many updates were taken. Later assertions win by
    /// `effective_from_ms`, so a feed that replays or reorders cannot walk an
    /// account's standing backwards — the map is a high-water mark, not a
    /// queue. Nothing is ever removed from it for the same reason
    /// `AccountInvalidations` retains its entries: the party able to make a
    /// feed go quiet must not thereby hold a mass-pardon lever.
    pub fn poll(&self) -> usize {
        let Some(feed) = self.feed.as_ref() else {
            return 0;
        };
        let drained = feed.drain();
        if drained.is_empty() {
            return 0;
        }
        let mut latest = self.latest.write().unwrap_or_else(|e| e.into_inner());
        for update in &drained {
            latest
                .entry(update.account)
                .and_modify(|held| {
                    if update.effective_from_ms >= held.effective_from_ms {
                        *held = *update;
                    }
                })
                .or_insert(*update);
        }
        drained.len()
    }

    /// The standing a session *should* be carrying, or `None` to leave it
    /// alone.
    ///
    /// `issued_at_ms` is the session token's signed issuance instant and
    /// `current` is the standing the session carries at this moment — not
    /// necessarily the token's, because an earlier sweep may already have
    /// moved it. Returning `None` for "already there" is what keeps a sweep
    /// idempotent and its counters honest.
    ///
    /// This **counts nothing and decides nothing**. The caller holds C5's
    /// posture, applies or does not, and then calls [`Self::record_applied`]
    /// or [`Self::record_observed`]. Splitting it this way is what keeps one
    /// control on one lever.
    pub fn pending(
        &self,
        account: AccountId,
        issued_at_ms: UnixMillis,
        current: SessionStanding,
    ) -> Option<SessionStanding> {
        let latest = self.latest.read().unwrap_or_else(|e| e.into_inner());
        let held = latest.get(&account)?;
        if issued_at_ms >= held.effective_from_ms {
            return None;
        }
        (held.standing != current).then_some(held.standing)
    }

    /// [`Self::pending`] for a caller holding verified claims and no
    /// separately tracked current standing — an admission, where the token's
    /// own standing is what the session would otherwise carry.
    pub fn pending_for(&self, claims: &SessionTokenClaimsV1) -> Option<SessionStanding> {
        self.pending(claims.account, claims.issued_at_ms, claims.standing)
    }

    /// Record that a pending change was applied (C5 `Live`).
    pub fn record_applied(&self) {
        self.applied.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that a pending change was evaluated and deliberately not applied
    /// (C5 `Shadow`).
    pub fn record_observed(&self) {
        self.observed.fetch_add(1, Ordering::Relaxed);
    }

    /// How many standing changes this consumer's callers have applied.
    #[must_use]
    pub fn applied(&self) -> u64 {
        self.applied.load(Ordering::Relaxed)
    }

    /// How many they would have applied in an acting posture.
    #[must_use]
    pub fn observed(&self) -> u64 {
        self.observed.load(Ordering::Relaxed)
    }
}

/// A feed backed by a queue a publisher — or a harness — pushes into.
///
/// The publisher half of the mechanism does not exist yet: identity has a
/// scorer but no service that writes the durable row a fleet-wide poller would
/// read (D33 clause (a) gives that writer to identity). Until it lands this is
/// the only feed in the tree, which is the other reason every default is
/// inert. `StandingInvalidationFeed`'s doc comment says the same thing about
/// the termination half.
#[derive(Debug, Default)]
pub struct QueuedStandingUpdates(Mutex<Vec<AccountStandingUpdate>>);

impl QueuedStandingUpdates {
    /// An empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish one update to whichever consumer drains this queue next.
    pub fn publish(&self, update: AccountStandingUpdate) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(update);
    }
}

impl StandingUpdateFeed for QueuedStandingUpdates {
    fn drain(&self) -> Vec<AccountStandingUpdate> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(account: u64, standing: SessionStanding, from: u64) -> AccountStandingUpdate {
        AccountStandingUpdate {
            account: AccountId(account),
            standing,
            effective_from_ms: UnixMillis::new(from),
        }
    }

    fn consumer() -> (Arc<QueuedStandingUpdates>, AccountStandings) {
        let feed = Arc::new(QueuedStandingUpdates::new());
        let standings = AccountStandings::new(Arc::clone(&feed) as Arc<dyn StandingUpdateFeed>);
        (feed, standings)
    }

    #[test]
    fn an_update_reaches_a_token_signed_before_its_watermark() {
        let (feed, standings) = consumer();
        feed.publish(update(7, SessionStanding::Quarantined, 5_000));
        assert_eq!(standings.poll(), 1);
        assert_eq!(
            standings.pending(AccountId(7), UnixMillis::new(4_999), SessionStanding::Good),
            Some(SessionStanding::Quarantined)
        );
    }

    #[test]
    fn a_token_signed_at_or_after_the_watermark_stands_on_its_own_merits() {
        let (feed, standings) = consumer();
        feed.publish(update(7, SessionStanding::Quarantined, 5_000));
        standings.poll();
        // Identity answered for this account again at exactly the watermark;
        // whatever it signed then is the current answer.
        assert_eq!(
            standings.pending(AccountId(7), UnixMillis::new(5_000), SessionStanding::Good),
            None
        );
    }

    #[test]
    fn lifting_is_the_same_code_path_as_applying() {
        let (feed, standings) = consumer();
        feed.publish(update(7, SessionStanding::Good, 5_000));
        standings.poll();
        assert_eq!(
            standings.pending(
                AccountId(7),
                UnixMillis::new(1_000),
                SessionStanding::Quarantined
            ),
            Some(SessionStanding::Good)
        );
    }

    #[test]
    fn a_standing_already_in_force_is_not_pending() {
        let (feed, standings) = consumer();
        feed.publish(update(7, SessionStanding::Quarantined, 5_000));
        standings.poll();
        assert_eq!(
            standings.pending(
                AccountId(7),
                UnixMillis::new(1_000),
                SessionStanding::Quarantined
            ),
            None
        );
    }

    #[test]
    fn the_map_is_a_high_water_mark_so_a_replayed_feed_cannot_walk_it_back() {
        let (feed, standings) = consumer();
        feed.publish(update(7, SessionStanding::Quarantined, 5_000));
        standings.poll();
        // An older assertion arriving late — a retry, a reordered fan-out.
        feed.publish(update(7, SessionStanding::Good, 4_000));
        standings.poll();
        assert_eq!(
            standings.pending(AccountId(7), UnixMillis::new(1_000), SessionStanding::Good),
            Some(SessionStanding::Quarantined)
        );
    }

    #[test]
    fn the_two_counters_are_separate_ledgers() {
        let (_feed, standings) = consumer();
        standings.record_observed();
        standings.record_observed();
        standings.record_applied();
        assert_eq!(standings.observed(), 2);
        assert_eq!(standings.applied(), 1);
    }

    #[test]
    fn an_inert_consumer_polls_and_resolves_nothing() {
        let standings = AccountStandings::inert();
        assert_eq!(standings.poll(), 0);
        assert_eq!(
            standings.pending(AccountId(7), UnixMillis::new(1), SessionStanding::Good),
            None
        );
    }
}
