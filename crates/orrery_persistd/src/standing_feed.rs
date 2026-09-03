//! The gateway's read of identity's durable cooldown entries (D33 clause (e)).
//!
//! [`crate::gateway::GatewayConfig::standing_feed`] is the seam through which a
//! gateway learns which accounts identity has invalidated. Until now its only
//! non-test setter did not exist: the producer type lives in `orrery_identity`,
//! `orrery_identity` depends on `orrery_persistd`, and `[[bin]] persistd` lives
//! *inside* `crates/orrery_persistd`, so the composition root cannot name the
//! producer. `docs/spikes/862-gateway-consumer-dependency-cycle.md` carries the
//! cargo error, including the finding that `optional = true` does not dodge it —
//! cargo resolves the package graph before it resolves features.
//!
//! [`DcCooldownFeed`] is the accepted answer (#862, owner decision 2026-09-03):
//! the feed needs identity's `dc` **rows**, not identity's **types**, so it
//! reads them through [`crate::keyspace`] and imports nothing from identity.
//!
//! # This is a read, not a second writer
//!
//! D31 clause (b) gives identity sole ownership of the `d` family's *writes*,
//! and nothing here writes. It is the posture the coordinator already ships and
//! states at `crates/orrery_coordinator/src/standing_feed.rs:20-24` — "a *read*
//! of a family this process never writes, which keeps D31's sole-writer rule
//! intact". The difference is only the depth: the coordinator reads through
//! identity's typed `AccountStore` because it is allowed to link identity, and
//! this reads the raw keyspace because a gateway is not. That is the accepted
//! cost of the decision, and [`crate::keyspace::cooldown_entry_key`] is what
//! bounds it — the layout has exactly one definition, in the module that owns
//! the family.
//!
//! # Why `dc` and not the `ya` strike family
//!
//! The watermark this publishes must be **monotone upward per account**, and it
//! must be the instant the refusal began. `dc.entered_at_ms` *is* that instant,
//! recorded once when the account crossed `C`. Re-scoring the `ya` strike rows
//! at poll time would instead yield a *later* instant on every poll, and a
//! watermark that walks forward kills tokens the account legitimately holds:
//! [`crate::gateway::AccountInvalidations`] invalidates every token minted
//! before the watermark, so a watermark drifting toward `now` eventually
//! invalidates everything, including tokens identity minted *after* answering
//! for the account again.

use std::sync::Arc;

use futures::TryStreamExt as _;
use orrery_protocol::{AccountId, AccountInvalidation, UnixMillis};

use crate::gateway::{FeedFailure, StandingInvalidationFeed};
use crate::keyspace;

/// D33 clause (e)'s invalidations, read straight from identity's durable `dc`
/// family.
///
/// Construct one per process and share it through
/// [`crate::gateway::GatewayConfig::standing_feed`]; what the gateway *does*
/// with the entries is [`crate::gateway::GatewayConfig::strikes_posture`]'s
/// decision, not this type's. A feed with C5 at `Off` is never even polled.
#[derive(Clone)]
pub struct DcCooldownFeed {
    db: Arc<foundationdb::Database>,
}

impl core::fmt::Debug for DcCooldownFeed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DcCooldownFeed").finish_non_exhaustive()
    }
}

impl DcCooldownFeed {
    /// A feed reading the `dc` family of `db`.
    #[must_use]
    pub fn from_database(db: Arc<foundationdb::Database>) -> Self {
        Self { db }
    }

    /// A feed reading the `dc` family of `context`'s cluster.
    #[must_use]
    pub fn from_context(context: &crate::fdb::FdbContext) -> Self {
        Self::from_database(context.database())
    }
}

#[async_trait::async_trait]
impl StandingInvalidationFeed for DcCooldownFeed {
    async fn invalidations(&self) -> Result<Vec<AccountInvalidation>, FeedFailure> {
        let start = keyspace::cooldown_range_start();
        let end = keyspace::cooldown_range_end();
        self.db
            .run(|trx, _maybe_committed| {
                let (start, end) = (start.clone(), end.clone());
                async move {
                    // A snapshot read, for the reason identity's own sweep
                    // gives at `orrery_identity/src/fdb.rs`'s
                    // `cooldown_entries`: this is a reporting poll, not an
                    // admission decision, and taking read conflict ranges over
                    // the whole family would make every poll conflict with
                    // every concurrent `observe_cooldown`. The feed contract
                    // already tolerates a poll being one interval stale.
                    let mut stream = trx.get_ranges_keyvalues(
                        foundationdb::RangeOption {
                            begin: foundationdb::KeySelector::first_greater_or_equal(start),
                            end: foundationdb::KeySelector::first_greater_or_equal(end),
                            ..foundationdb::RangeOption::default()
                        },
                        true,
                    );
                    let mut out = Vec::new();
                    while let Some(kv) = stream.try_next().await? {
                        // A malformed row is skipped, not fatal: refusing the
                        // whole poll over one unreadable row would degrade a
                        // fleet to *no* enforcement, which is the direction
                        // D33 clause (f) forbids. The `dc` family has exactly
                        // one writer and one layout, so this is unreachable
                        // short of corruption — and corruption of one row must
                        // not pardon every other account in the family.
                        let (Ok(key), Ok(value)): (Result<[u8; 10], _>, Result<[u8; 8], _>) =
                            (kv.key().try_into(), kv.value().try_into())
                        else {
                            tracing::warn!(
                                key = ?kv.key(),
                                "gateway: skipping a malformed dc cooldown row"
                            );
                            continue;
                        };
                        let mut account = [0u8; 8];
                        account.copy_from_slice(&key[2..]);
                        out.push(AccountInvalidation {
                            account: AccountId(u64::from_be_bytes(account)),
                            effective_from_ms: UnixMillis(u64::from_be_bytes(value)),
                        });
                    }
                    Ok(out)
                }
            })
            .await
            // A store failure becomes a feed failure, never an empty set: an
            // unreachable cluster reported as "nobody is invalidated" is the
            // fleet-wide pardon D33 clause (f) forbids, and the consumer's
            // documented behaviour on `Err` is to keep the entries it already
            // holds — stale enforcement rather than none.
            .map_err(|error| FeedFailure(error.to_string()))
    }
}
