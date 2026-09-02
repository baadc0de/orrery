//! D33 clause (e)'s account-generation invalidation publisher.
//!
//! Clause (e): "On a change to cooldown or ban, identity publishes an account
//! generation invalidation and gateways terminate matching sessions." The
//! consumers of that publication have existed for some time — `orrery_persistd`'s
//! `AccountInvalidations` and `orrery_coordinator`'s `StandingState`, each with
//! its own `StandingInvalidationFeed` trait — but nothing in the tree produced
//! an [`AccountInvalidation`] outside a test double. This module is the
//! producer.
//!
//! # Why this reads `dc`, and not `ya`
//!
//! [`AccountInvalidation::effective_from_ms`] is defined as "identity's read
//! instant when the refusal began". That instant is already durable: it is the
//! `entered_at_ms` of the `dc ‖ account` cooldown entry
//! ([`crate::store::CooldownEntry`]), written by
//! [`AccountStore::observe_cooldown`] at the exact moment
//! [`crate::cooldown`] refused the account.
//!
//! Re-deriving it by scoring the executor-owned `ya` strike family instead
//! would be both more expensive and *wrong*. More expensive because standing
//! is a per-account range read (D33 clause (f)), so a fleet-wide sweep would
//! be one range read per account rather than one for the family. Wrong because
//! a score computed at poll time yields the *poll's* instant, which is later
//! than the refusal — and a watermark that is too late kills tokens the
//! account minted legitimately before it, which is precisely the
//! over-enforcement the watermark rule exists to avoid.
//!
//! The `dc` family is also exactly the right *set*: [`crate::cooldown`] writes
//! an entry for `Cooldown` and `Banned` and for no other level, and clears it
//! on release through `clear_cooldown_if`. Membership is the predicate, so
//! this module holds no threshold of its own and cannot disagree with the
//! scorer about who is refused.
//!
//! # What this does not claim
//!
//! **Coverage is mint-driven.** A `dc` row appears when identity *observes* an
//! account at or above `C`, which happens when that account attempts a mint or
//! refresh. Clause (e) says standing is evaluated "after every live filing and
//! whenever identity mints or refreshes a token"; this publisher delivers the
//! second half only. An account that crosses `C` and never returns to identity
//! is not published until it does — its existing tokens expire on their own
//! signed TTLs (at most `MAX_SESSION_TOKEN_TTL_MS`, one hour) in the meantime.
//! Closing the first half requires an identity service half that reacts to
//! filings, which does not exist in this tree; it is not invented here.
//!
//! **Absence is not a retraction, and this producer relies on the consumers
//! for that.** A released account's row is cleared, so it leaves this
//! response — and both consumers deliberately never remove an applied entry
//! on that basis (see `StandingInvalidationFeed::invalidations`'s contract).
//! Recovery runs through minting: a token issued at or after the watermark was
//! signed by an identity answering for the account again.

use crate::store::{AccountStore, IdentityError};
use orrery_protocol::{AccountInvalidation, UnixMillis};

/// Publishes the invalidations implied by the identity store's `dc` family.
///
/// Holds the store by value so a caller may pass an owned handle or a shared
/// one: `AccountStore` is implemented for `Arc<T>`, which is how a single
/// store backs both a login path and this publisher.
#[derive(Debug, Clone, Copy)]
pub struct StandingInvalidationSource<S> {
    store: S,
}

impl<S> StandingInvalidationSource<S> {
    /// Publish from this account store.
    pub const fn new(store: S) -> Self {
        Self { store }
    }

    /// The store this publisher reads.
    pub const fn store(&self) -> &S {
        &self.store
    }
}

impl<S: AccountStore> StandingInvalidationSource<S> {
    /// Every invalidation currently in force.
    ///
    /// The full set every call, not a delta: the feed contract is stateless so
    /// that a restarted consumer converges on its first poll and a lost poll
    /// costs nothing but the next one.
    ///
    /// # Errors
    ///
    /// Propagates the store's failure unchanged. A caller must surface this as
    /// a feed failure rather than as an empty set — an empty successful
    /// response and a failed read are different facts, and only the first one
    /// means "nobody is refused".
    pub async fn current(&self) -> Result<Vec<AccountInvalidation>, IdentityError> {
        Ok(self
            .store
            .cooldown_entries()
            .await?
            .into_iter()
            .map(|record| AccountInvalidation {
                account: record.account,
                effective_from_ms: UnixMillis(record.entry.entered_at_ms),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemAccountStore;
    use orrery_protocol::AccountId;
    use std::sync::Arc;

    const ALICE: AccountId = AccountId(0x0862_0000_0000_0001);
    const BOB: AccountId = AccountId(0x0862_0000_0000_0002);

    /// `observe_cooldown` refuses an account the store has never seen, so a
    /// fixture has to create the `da` rows before it can cool anything down.
    async fn store_with_accounts() -> Arc<MemAccountStore> {
        let store = Arc::new(MemAccountStore::new());
        store.create_account(ALICE, 0).await.expect("create alice");
        store.create_account(BOB, 0).await.expect("create bob");
        store
    }

    #[tokio::test]
    async fn an_empty_family_publishes_nothing() {
        let store = store_with_accounts().await;
        let published = StandingInvalidationSource::new(store)
            .current()
            .await
            .expect("read the empty family");
        assert!(
            published.is_empty(),
            "no account is in cooldown, so nobody's tokens are invalidated"
        );
    }

    #[tokio::test]
    async fn an_entry_publishes_its_own_entry_instant_as_the_watermark() {
        let store = store_with_accounts().await;
        store
            .observe_cooldown(ALICE, 5_000, None)
            .await
            .expect("alice enters cooldown");

        let published = StandingInvalidationSource::new(Arc::clone(&store))
            .current()
            .await
            .expect("publish");

        assert_eq!(
            published,
            vec![AccountInvalidation {
                account: ALICE,
                effective_from_ms: UnixMillis(5_000),
            }],
            "the watermark is the refusal instant, not the poll instant"
        );
    }

    /// The mutation this whole module exists to prevent: publishing the poll's
    /// clock instead of the entry's would invalidate tokens minted between the
    /// refusal and the poll, which the account was entitled to hold.
    #[tokio::test]
    async fn the_watermark_does_not_move_when_the_clock_does() {
        let store = store_with_accounts().await;
        store
            .observe_cooldown(ALICE, 5_000, None)
            .await
            .expect("alice enters cooldown");
        let source = StandingInvalidationSource::new(Arc::clone(&store));

        let first = source.current().await.expect("first poll");
        // A later poll, with no new strike, must republish the same watermark.
        store
            .observe_cooldown(ALICE, 900_000, None)
            .await
            .expect("a later observation of the same cooldown");
        let second = source.current().await.expect("second poll");

        assert_eq!(
            first, second,
            "re-observing an unchanged cooldown must not advance the watermark"
        );
        assert_eq!(first[0].effective_from_ms, UnixMillis(5_000));
    }

    #[tokio::test]
    async fn a_new_live_strike_restarts_the_watermark() {
        let store = store_with_accounts().await;
        store
            .observe_cooldown(ALICE, 5_000, None)
            .await
            .expect("alice enters cooldown");
        // A strike issued after the entry restarts the interval, and the
        // watermark has to follow it: tokens minted during the first interval
        // must not survive the second.
        store
            .observe_cooldown(ALICE, 900_000, Some(800_000))
            .await
            .expect("a newer live strike restarts cooldown");

        let published = StandingInvalidationSource::new(Arc::clone(&store))
            .current()
            .await
            .expect("publish");

        assert_eq!(published[0].effective_from_ms, UnixMillis(900_000));
    }

    #[tokio::test]
    async fn a_released_account_leaves_the_published_set() {
        let store = store_with_accounts().await;
        let entry = store
            .observe_cooldown(ALICE, 5_000, None)
            .await
            .expect("alice enters cooldown");
        store
            .observe_cooldown(BOB, 6_000, None)
            .await
            .expect("bob enters cooldown");
        assert!(store
            .clear_cooldown_if(ALICE, entry)
            .await
            .expect("release alice"));

        let published = StandingInvalidationSource::new(Arc::clone(&store))
            .current()
            .await
            .expect("publish");

        // Consumers never treat this absence as a retraction; that rule is
        // theirs and is tested where it lives. What this asserts is only that
        // the producer stops asserting a refusal it no longer holds.
        assert_eq!(
            published,
            vec![AccountInvalidation {
                account: BOB,
                effective_from_ms: UnixMillis(6_000),
            }]
        );
    }

    #[tokio::test]
    async fn every_cooled_account_is_published_in_one_poll() {
        let store = store_with_accounts().await;
        store
            .observe_cooldown(ALICE, 5_000, None)
            .await
            .expect("alice");
        store.observe_cooldown(BOB, 6_000, None).await.expect("bob");

        let published = StandingInvalidationSource::new(Arc::clone(&store))
            .current()
            .await
            .expect("publish");

        assert_eq!(
            published,
            vec![
                AccountInvalidation {
                    account: ALICE,
                    effective_from_ms: UnixMillis(5_000),
                },
                AccountInvalidation {
                    account: BOB,
                    effective_from_ms: UnixMillis(6_000),
                },
            ],
            "the feed is the full set every call, so one poll converges a \
             restarted consumer"
        );
    }
}
