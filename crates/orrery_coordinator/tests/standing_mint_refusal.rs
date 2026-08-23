//! D33 clause (e)'s first half, proven end to end: identity **refuses to
//! mint** a session token for a cooled-down or banned account, while a
//! quarantined one still mints and a cooldown lapses by decay alone.
//!
//! # Why this lives in `orrery_coordinator`'s tests
//!
//! The scorer and the refusal are `orrery_identity` code (#266), and that
//! crate was held by another lane while #219 ran — so the proof drives its
//! public API from outside rather than editing files the holder owns. The
//! dependency direction is legal (coordinator → identity → persistd) and the
//! placement is temporary: when the identity lane is free, move this file to
//! `orrery_identity/tests/` beside `issuance.rs`, whose fixtures it borrows.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use orrery_identity::issuer::{IssuerKeyring, IssuerSigningKey};
use orrery_identity::mem::MemAccountStore;
use orrery_identity::store::{AccountStore, BindOutcome};
use orrery_identity::{
    ComputedStanding, IdentityError, IdentityService, StaticStrikeRows, DEFAULT_STANDING_THRESHOLDS,
};
use orrery_persistd::adjudication::{
    StrikeEvidenceRef, StrikeKind, StrikeMode, StrikeRow, MAJOR_STRIKE_WEIGHT_MILLI,
    STRIKE_RETENTION_MS,
};
use orrery_protocol::{
    AccountId, FixedTokenClock, IssuerKeyId, NodeId, PersistId, RulesetId, SessionStanding, Tick,
    UnixMillis,
};

const ACCOUNT: AccountId = AccountId::new(7);
const T0: u64 = 1_000_000_000;
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

fn node(seed: u8) -> NodeId {
    iroh::SecretKey::from_bytes(&[seed; 32]).public()
}

/// One live major finding at `issued_at_ms` (D33 clause (a)'s 3.0 weight).
fn row(issued_at_ms: u64, mode: StrikeMode) -> StrikeRow {
    StrikeRow {
        issued_at_ms,
        weight_milli: MAJOR_STRIKE_WEIGHT_MILLI,
        kind: StrikeKind::Deviation,
        evidence_ref: StrikeEvidenceRef {
            entity: PersistId::new(1),
            window_start: Tick::new(1),
            window_end: Tick::new(2),
            digest: [issued_at_ms as u8; 32],
        },
        ruleset: RulesetId {
            version: 1,
            digest: [1; 32],
        },
        mode,
        expires_at_ms: issued_at_ms + STRIKE_RETENTION_MS,
    }
}

/// The service over a real store and the real scorer, with the ledger's read
/// instant driven by `now` so a test can age the ledger without an operator.
async fn service_with(
    rows: Vec<StrikeRow>,
    now: Arc<AtomicU64>,
) -> (
    IdentityService<
        MemAccountStore,
        ComputedStanding<StaticStrikeRows, impl Fn() -> u64 + Send + Sync>,
        FixedTokenClock,
    >,
    NodeId,
) {
    let store = MemAccountStore::new();
    store
        .create_account(ACCOUNT, T0)
        .await
        .expect("create account");
    let bound = node(9);
    assert_eq!(
        store.bind(ACCOUNT, &bound, T0).await.expect("bind"),
        BindOutcome::Bound
    );
    let scorer_now = Arc::clone(&now);
    let service = IdentityService::new(
        store,
        ComputedStanding::new(
            StaticStrikeRows::new([(ACCOUNT, rows)]),
            move || scorer_now.load(Ordering::SeqCst),
            DEFAULT_STANDING_THRESHOLDS,
        )
        .expect("default standing thresholds are coherent"),
        FixedTokenClock::new(UnixMillis::new(T0)),
        IssuerKeyring::new(IssuerSigningKey::new(
            IssuerKeyId::new(1),
            iroh::SecretKey::from_bytes(&[8; 32]),
        )),
    );
    (service, bound)
}

/// Clause (e)'s middle rung is admission, not a token state: identity refuses
/// to mint, and the error names the reason rather than collapsing into a bad
/// credential.
#[tokio::test]
async fn identity_refuses_to_mint_for_a_cooled_down_account() {
    // Two major findings inside decay's escalation window clear C = 5.0.
    let now = Arc::new(AtomicU64::new(T0));
    let (service, bound) = service_with(vec![row(T0, StrikeMode::Live); 2], Arc::clone(&now)).await;

    match service.issue(ACCOUNT, &bound, None).await {
        Err(IdentityError::Cooldown(cooled)) => assert_eq!(cooled, ACCOUNT),
        other => panic!("expected a mint refusal naming cooldown, got {other:?}"),
    }
}

/// The top rung likewise: refusal at issuance, distinguishable from ban by
/// the error's own variant.
#[tokio::test]
async fn identity_refuses_to_mint_for_a_banned_account() {
    // Three majors reach B = 7.0 outright.
    let now = Arc::new(AtomicU64::new(T0));
    let (service, bound) = service_with(vec![row(T0, StrikeMode::Live); 3], Arc::clone(&now)).await;

    match service.issue(ACCOUNT, &bound, None).await {
        Err(IdentityError::Banned(banned)) => assert_eq!(banned, ACCOUNT),
        other => panic!("expected a mint refusal naming ban, got {other:?}"),
    }
}

/// Quarantine is not a ban, and the ladder must not collapse upward: the one
/// case where a token *is* minted with a non-`Good` standing keeps working.
#[tokio::test]
async fn a_quarantined_account_is_still_minted_its_token() {
    // One major sits in [Q, C): quarantined, mintable.
    let now = Arc::new(AtomicU64::new(T0));
    let (service, bound) = service_with(vec![row(T0, StrikeMode::Live)], Arc::clone(&now)).await;

    let issued = service
        .issue(ACCOUNT, &bound, None)
        .await
        .expect("quarantined is not a refusal");
    assert_eq!(issued.claims().standing, SessionStanding::Quarantined);
}

/// Shadow rows never cross a threshold (D32 clause (d)): no live row means
/// `S_live = 0`, so a shadow-period ledger cannot cool anybody down.
#[tokio::test]
async fn shadow_rows_never_reach_a_refusal() {
    let now = Arc::new(AtomicU64::new(T0));
    let (service, bound) =
        service_with(vec![row(T0, StrikeMode::Shadow); 3], Arc::clone(&now)).await;

    let issued = service
        .issue(ACCOUNT, &bound, None)
        .await
        .expect("shadow rows count nothing");
    assert_eq!(issued.claims().standing, SessionStanding::Good);
}

/// A cooldown has a duration and it lapses on its own: the same two findings,
/// aged past decay's fall below Q by nothing but the injected clock, mint a
/// `Good` token again. No operator acts between the two reads.
#[tokio::test]
async fn a_cooldown_lapses_by_decay_driven_by_the_injected_clock() {
    let now = Arc::new(AtomicU64::new(T0));
    let (service, bound) = service_with(vec![row(T0, StrikeMode::Live); 2], Arc::clone(&now)).await;

    // Ten days in, decay has left ~3.66 points: below C, above Q, so the
    // account has cooled down into quarantine — minted, but flagged.
    now.store(T0 + 10 * DAY_MS, Ordering::SeqCst);
    let issued = service
        .issue(ACCOUNT, &bound, None)
        .await
        .expect("below C mints again");
    assert_eq!(issued.claims().standing, SessionStanding::Quarantined);

    // Fifteen days in, both findings have shed more than half their weight:
    // under Q entirely, so the standing is ordinary again.
    now.store(T0 + 15 * DAY_MS, Ordering::SeqCst);
    let issued = service
        .issue(ACCOUNT, &bound, None)
        .await
        .expect("past Q the ladder releases");
    assert_eq!(issued.claims().standing, SessionStanding::Good);
}
