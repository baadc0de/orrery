//! Adapts identity's invalidation publisher to this crate's feed trait.
//!
//! The producer itself is `orrery_identity::StandingInvalidationSource`, which
//! is where it belongs: D31 gives identity sole ownership of the `d` family,
//! and D33 clause (e) names identity as the publisher. This module is only the
//! seam that lets a coordinator poll it in-process.
//!
//! # Why the adapter is here and not there
//!
//! [`crate::server::StandingInvalidationFeed`] is deliberately a *local* trait
//! — `orrery_persistd`'s gateway declares its own copy for the same reason, so
//! neither consumer's polling contract is coupled to the other's. Implementing
//! it for identity's type therefore has to happen in one of the two crates,
//! and only one direction is acyclic: `orrery_identity` already depends on
//! `orrery_persistd`, so identity cannot depend on a consumer without closing
//! a loop. The adapter lives on the consumer side.
//!
//! # In-process, not over the wire
//!
//! This is a coordinator reading identity's store directly, which is honest
//! about what exists today rather than pretending there is an identity service
//! to call: `server.rs` records that "the publisher is identity's scorer,
//! whose service half does not exist yet". It is a *read* of a family this
//! process never writes, which keeps D31's sole-writer rule intact. When the
//! service half lands, the feed behind this trait changes and neither
//! consumer's polling code does.

use crate::server::{FeedFailure, StandingInvalidationFeed};
use orrery_identity::{AccountStore, StandingInvalidationSource};
use orrery_protocol::AccountInvalidation;

/// A [`StandingInvalidationFeed`] backed by an identity account store.
#[derive(Debug, Clone, Copy)]
pub struct IdentityStandingFeed<S> {
    source: StandingInvalidationSource<S>,
}

impl<S> IdentityStandingFeed<S> {
    /// Poll this identity store for D33 clause (e)'s invalidations.
    pub const fn new(store: S) -> Self {
        Self {
            source: StandingInvalidationSource::new(store),
        }
    }
}

#[async_trait::async_trait]
impl<S: AccountStore> StandingInvalidationFeed for IdentityStandingFeed<S> {
    async fn invalidations(&self) -> Result<Vec<AccountInvalidation>, FeedFailure> {
        // A store failure becomes a feed failure, never an empty set. The
        // sweep's contract is that a failed poll keeps the entries it already
        // applied; reporting "nobody is invalidated" instead would turn an
        // unreachable identity store into a fleet-wide pardon, which is the
        // same failed-open shape D33 clause (f) forbids on the mint path.
        self.source
            .current()
            .await
            .map_err(|error| FeedFailure(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_identity::MemAccountStore;
    use orrery_protocol::{AccountId, UnixMillis};
    use std::sync::Arc;

    const ACCOUNT: AccountId = AccountId(0x0862_0000_0000_00fe);

    #[tokio::test]
    async fn a_cooldown_entry_reaches_the_coordinator_as_an_invalidation() {
        let store = Arc::new(MemAccountStore::new());
        store
            .create_account(ACCOUNT, 0)
            .await
            .expect("create the account");
        store
            .observe_cooldown(ACCOUNT, 4_242, None)
            .await
            .expect("the account crosses C");

        let feed = IdentityStandingFeed::new(Arc::clone(&store));

        assert_eq!(
            feed.invalidations().await.expect("poll"),
            vec![AccountInvalidation {
                account: ACCOUNT,
                effective_from_ms: UnixMillis(4_242),
            }],
            "crossing C must reach the coordinator's feed with the refusal instant"
        );
    }

    #[tokio::test]
    async fn an_untroubled_store_publishes_nothing() {
        let store = Arc::new(MemAccountStore::new());
        let feed = IdentityStandingFeed::new(store);
        assert!(feed.invalidations().await.expect("poll").is_empty());
    }
}
