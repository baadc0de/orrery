//! #862 box 2 item 3 — can persistd's gateway consume standing invalidations?
//!
//! Propose-only. Nothing in the workspace can reach this file; see `README.md`
//! for why the manifest ships as `Cargo.toml.txt` and how to run it.
//!
//! # What is being tested
//!
//! `GatewayConfig::standing_feed` (`crates/orrery_persistd/src/gateway.rs:3394`)
//! is set nowhere outside `crates/orrery_persistd/tests/gateway_standing.rs`.
//! The recorded reason is a dependency cycle: `orrery_identity` depends on
//! `orrery_persistd` and never the reverse, so the producer
//! (`orrery_identity::StandingInvalidationSource`) cannot be adapted inside
//! persistd the way `orrery_coordinator::standing_feed::IdentityStandingFeed`
//! adapts it for the coordinator.
//!
//! That reason is correct — `README.md` carries the cargo error — but it is
//! about the *producer type*, not about the *data*. This spike tests the
//! narrower claim that follows:
//!
//! > A gateway-side feed needs identity's `dc` **rows**, not identity's
//! > **types**. Reading them requires no new edge on the spine at all.
//!
//! [`DcCooldownFeed`] below is the artifact. It implements
//! `orrery_persistd::gateway::StandingInvalidationFeed` using only
//! `orrery_persistd` and `orrery_protocol`; it imports nothing from
//! `orrery_identity`, and a `use orrery_identity::…` inside it would be
//! deleted by the compiler check that matters — a real `persistd` binary
//! cannot name that crate.
//!
//! The harness around it uses `orrery_identity::FdbAccountStore` as the
//! *writer*, standing in for the deployed `orrery-identity` binary, so the
//! bytes the feed reads are the bytes identity really produces rather than
//! bytes this file invented.
//!
//! # What it deliberately does not do
//!
//! No enforcement. The feed is polled and printed; it is never installed on a
//! `GatewayConfig`, and no `StrikesPosture` is consulted. Wiring a consumer
//! makes the gateway an enforcement point, and #934 found a live bug from an
//! enforcement arm that read no ramp posture. A spike that skipped posture and
//! was then promoted is exactly how that recurs, so the posture read is left
//! undone and visible rather than half-done and plausible.

use futures::TryStreamExt as _;
use orrery_persistd::gateway::{FeedFailure, StandingInvalidationFeed};
use orrery_protocol::{AccountId, AccountInvalidation, UnixMillis};
use std::sync::Arc;

/// Inclusive start of D33's cooldown-entry family, `dc`.
///
/// The layout is `dc ‖ account:u64-be` -> `entered_at_ms:u64-be`: a ten-byte
/// key and a fixed eight-byte big-endian value, with no postcard framing.
///
/// **These four constants are the whole cost of this candidate**, and in the
/// recommended form they do not live here at all. `orrery_persistd::keyspace`
/// already owns every other bound in the `d` family — `account_range_start`
/// (`da`, `keyspace.rs:1409`), `binding_range_start`/`_end` (`db`, `:1421`,
/// `:1429`) and `binding_history_range_start`/`_end` (`dh`, `:1437`, `:1450`)
/// — and `keyspace.rs:1429` already documents `dc` by name as "the gap the
/// discriminators `a < b < h` deliberately leave". Identity's
/// `cooldown_entry_key` (`crates/orrery_identity/src/fdb.rs:279`) is the one
/// `d`-family key builder in the tree that sits outside that module, and it is
/// private. So the production form of this candidate *moves* the builder to
/// the module that owns its five siblings; it does not copy it. That matters,
/// because `crates/orrery_identity/Cargo.toml:29` records the rule this would
/// otherwise break: "a second copy of those bytes is the one thing D31 clause
/// (b) cannot survive."
const COOLDOWN_RANGE_START: [u8; 2] = [b'd', b'c'];

/// Exclusive end of the `dc` family — the successor of the two-byte prefix, so
/// `[dc, dd)` spans every ten-byte row and nothing else.
const COOLDOWN_RANGE_END: [u8; 2] = [b'd', b'd'];

/// D33 clause (e)'s invalidations, read straight from identity's durable `dc`
/// family.
///
/// # Why this is a read and not a violation of D31's sole-writer rule
///
/// D31 gives identity sole ownership of the `d` family's *writes*. This type
/// only reads, which is the same posture the coordinator already ships:
/// `crates/orrery_coordinator/src/standing_feed.rs:20-24` calls its own
/// version "a *read* of a family this process never writes, which keeps D31's
/// sole-writer rule intact". The difference is only the depth of the read —
/// the coordinator goes through identity's typed `AccountStore`, and this goes
/// through the raw keyspace, because the coordinator is allowed to link
/// identity and a gateway is not.
struct DcCooldownFeed {
    db: Arc<foundationdb::Database>,
}

#[async_trait::async_trait]
impl StandingInvalidationFeed for DcCooldownFeed {
    async fn invalidations(&self) -> Result<Vec<AccountInvalidation>, FeedFailure> {
        self.db
            .run(|trx, _maybe_committed| async move {
                // Snapshot, for the reason identity's own sweep gives at
                // `fdb.rs:638-643`: this is a reporting poll, not an admission
                // decision, and taking read conflict ranges over the whole
                // family would make every poll conflict with every concurrent
                // `observe_cooldown`.
                let mut stream = trx.get_ranges_keyvalues(
                    foundationdb::RangeOption {
                        begin: foundationdb::KeySelector::first_greater_or_equal(
                            COOLDOWN_RANGE_START.as_slice(),
                        ),
                        end: foundationdb::KeySelector::first_greater_or_equal(
                            COOLDOWN_RANGE_END.as_slice(),
                        ),
                        ..foundationdb::RangeOption::default()
                    },
                    true,
                );
                let mut out = Vec::new();
                while let Some(kv) = stream.try_next().await? {
                    let key: [u8; 10] = kv.key().try_into().expect("dc key is ten bytes");
                    let value: [u8; 8] = kv.value().try_into().expect("dc value is eight bytes");
                    let mut account = [0u8; 8];
                    account.copy_from_slice(&key[2..]);
                    out.push(AccountInvalidation {
                        account: AccountId(u64::from_be_bytes(account)),
                        effective_from_ms: UnixMillis(u64::from_be_bytes(value)),
                    });
                }
                Ok(out)
            })
            .await
            // A store failure becomes a feed failure, never an empty set: an
            // unreachable cluster reported as "nobody is invalidated" is the
            // fleet-wide pardon D33 clause (f) forbids.
            .map_err(|error| FeedFailure(error.to_string()))
    }
}

/// Account ids are picked in a range no other lane uses. The dev cluster at
/// 127.0.0.1:4500 is shared, and a colliding fixture turns a sibling's test red.
const ALICE: AccountId = AccountId(0x0862_0003_0000_0001);
const BOB: AccountId = AccountId(0x0862_0003_0000_0002);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cluster_file = std::env::var("FDB_CLUSTER_FILE")
        .unwrap_or_else(|_| "/etc/foundationdb/fdb.cluster".to_string());

    // The consumer half: persistd only.
    let context = orrery_persistd::fdb::FdbContext::connect(&cluster_file)?;
    let feed = DcCooldownFeed {
        db: context.database(),
    };

    // The producer half, standing in for the deployed `orrery-identity`.
    let store = Arc::new(orrery_identity::fdb::FdbAccountStore::from_database(
        context.database(),
    ));

    let before = feed.invalidations().await?;
    assert!(
        !before.iter().any(|i| i.account == ALICE || i.account == BOB),
        "the spike's account range is not clean: {before:?}"
    );

    {
        use orrery_identity::AccountStore as _;
        store.create_account(ALICE, 0).await?;
        store.create_account(BOB, 0).await?;
        // Only Alice crosses C. Bob is the control: an account identity knows
        // about but has never cooled down must not appear in the feed.
        store.observe_cooldown(ALICE, 1_756_000_000_000, None).await?;
    }

    let after = feed.invalidations().await?;
    let mine: Vec<_> = after
        .iter()
        .filter(|i| i.account == ALICE || i.account == BOB)
        .collect();

    assert_eq!(
        mine,
        vec![&AccountInvalidation {
            account: ALICE,
            effective_from_ms: UnixMillis(1_756_000_000_000),
        }],
        "the gateway-side feed must see exactly what identity wrote"
    );

    println!("PASS: a dc row written by orrery_identity was read by a persistd-only feed");
    println!("      {mine:?}");

    // Leave the shared cluster as it was found.
    context
        .database()
        .run(|trx, _| async move {
            for account in [ALICE, BOB] {
                let mut key = [0u8; 10];
                key[0] = b'd';
                key[1] = b'c';
                key[2..].copy_from_slice(&account.0.to_be_bytes());
                trx.clear(&key);
                let mut da = [0u8; 10];
                da[0] = b'd';
                da[1] = b'a';
                da[2..].copy_from_slice(&account.0.to_be_bytes());
                trx.clear(&da);
            }
            Ok(())
        })
        .await?;
    println!("      fixture rows cleared");

    Ok(())
}
