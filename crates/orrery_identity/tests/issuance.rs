//! The two halves, checked against each other.
//!
//! Every test here mints with [`orrery_identity::IdentityService`] and checks
//! with `orrery_protocol::SessionTokenVerifier` — the verifier that
//! the coordinator (`orrery_coordinator::server`) and the gateway
//! (`orrery_persistd::gateway::SessionTokenV1Authorizer`) already use, taken
//! unmodified. That is the point of the file: if the issuer and the verifier
//! ever disagree, one of them was adjusted to the other, and this suite is
//! where that shows up.

use orrery_identity::mem::MemAccountStore;
use orrery_identity::store::AccountStore;
use orrery_identity::store::{BindOutcome, IdentityError};
use orrery_identity::{
    IdentityService, IssuerKeyring, IssuerSigningKey, StaticStanding, UnavailableStanding,
};
use orrery_persistd::gateway::BindingAuthority;
use orrery_protocol::AccountId;
use orrery_protocol::{
    FixedTokenClock, IssuerKeyId, NodeId, SessionStanding, SessionTokenVerificationError,
    SessionTokenVerifier, UnixMillis, MAX_SESSION_TOKEN_TTL_MS,
};

const ACCOUNT: AccountId = AccountId(7);
const OTHER_ACCOUNT: AccountId = AccountId(8);
const T0: u64 = 1_700_000_000_000;

fn secret(seed: u8) -> iroh_base::SecretKey {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    bytes[31] = 0x11;
    iroh_base::SecretKey::from_bytes(&bytes)
}

fn node(seed: u8) -> NodeId {
    secret(seed).public()
}

fn signing_key(id: u32, seed: u8) -> IssuerSigningKey {
    IssuerSigningKey::new(IssuerKeyId::new(id), secret(seed))
}

/// A service holding one issuer key, one account, and one bound node.
async fn service_with_standing(
    standing: SessionStanding,
) -> (
    IdentityService<MemAccountStore, StaticStanding, FixedTokenClock>,
    NodeId,
) {
    let store = MemAccountStore::new();
    store.create_account(ACCOUNT, T0).await.expect("create");
    let node = node(1);
    assert_eq!(
        store.bind(ACCOUNT, &node, T0).await.expect("bind"),
        BindOutcome::Bound
    );
    let service = IdentityService::new(
        store,
        StaticStanding::new([(ACCOUNT, standing)], None),
        FixedTokenClock::new(UnixMillis::new(T0)),
        IssuerKeyring::new(signing_key(1, 0xA1)),
    );
    (service, node)
}

async fn service() -> (
    IdentityService<MemAccountStore, StaticStanding, FixedTokenClock>,
    NodeId,
) {
    service_with_standing(SessionStanding::Good).await
}

#[tokio::test]
async fn a_minted_token_verifies_under_the_protocol_verifier() {
    let (service, node) = service().await;
    let session = service.issue(ACCOUNT, &node, None).await.expect("issue");

    let verifier = SessionTokenVerifier::new(
        FixedTokenClock::new(UnixMillis::new(T0 + 1)),
        service.published_issuer_keys(),
    );
    let claims = verifier
        .verify(&session.encoded, &node)
        .expect("the verifier accepts what the issuer minted");

    assert_eq!(claims.account, ACCOUNT);
    assert_eq!(claims.node, node);
    assert_eq!(claims.issued_at_ms, UnixMillis::new(T0));
    assert_eq!(claims.ttl_ms.0, MAX_SESSION_TOKEN_TTL_MS);
    assert_eq!(claims.standing, SessionStanding::Good);
    // docs/09 §8: clients refresh at half-TTL.
    assert_eq!(
        session.refresh_at_ms,
        UnixMillis::new(T0 + MAX_SESSION_TOKEN_TTL_MS / 2)
    );
}

#[tokio::test]
async fn a_token_is_bound_to_the_node_it_was_minted_for() {
    let other = node(2);
    let (service, node) = service().await;
    let session = service.issue(ACCOUNT, &node, None).await.expect("issue");

    let verifier = SessionTokenVerifier::new(
        FixedTokenClock::new(UnixMillis::new(T0 + 1)),
        service.published_issuer_keys(),
    );
    assert_eq!(
        verifier.verify(&session.encoded, &other),
        Err(SessionTokenVerificationError::WrongNode),
        "presenting another node's token is refused, so the service binds the \
         node it was asked to"
    );
}

#[tokio::test]
async fn the_ttl_cap_is_enforced_at_issuance() {
    let (service, node) = service().await;

    // The cap itself is issuable; one millisecond past it is not.
    service
        .issue(ACCOUNT, &node, Some(MAX_SESSION_TOKEN_TTL_MS))
        .await
        .expect("the cap is a legal lifetime");
    assert_eq!(
        service
            .issue(ACCOUNT, &node, Some(MAX_SESSION_TOKEN_TTL_MS + 1))
            .await
            .expect_err("above the cap is refused"),
        IdentityError::TtlAboveCap {
            requested_ms: MAX_SESSION_TOKEN_TTL_MS + 1,
            cap_ms: MAX_SESSION_TOKEN_TTL_MS,
        }
    );
    assert_eq!(
        service
            .issue(ACCOUNT, &node, Some(0))
            .await
            .expect_err("a zero lifetime is expired the instant it is signed"),
        IdentityError::ZeroTtl
    );
}

#[tokio::test]
async fn a_token_is_refused_after_its_ttl() {
    let (service, node) = service().await;
    let ttl_ms = 60_000;
    let session = service
        .issue(ACCOUNT, &node, Some(ttl_ms))
        .await
        .expect("issue");

    // One millisecond before expiry, and at expiry. The verifier's boundary is
    // `now - issued_at >= ttl`, so the second instant is the first refusal.
    let live = SessionTokenVerifier::new(
        FixedTokenClock::new(UnixMillis::new(T0 + ttl_ms - 1)),
        service.published_issuer_keys(),
    );
    assert!(live.verify(&session.encoded, &node).is_ok());

    let expired = SessionTokenVerifier::new(
        FixedTokenClock::new(UnixMillis::new(T0 + ttl_ms)),
        service.published_issuer_keys(),
    );
    assert_eq!(
        expired.verify(&session.encoded, &node),
        Err(SessionTokenVerificationError::Expired)
    );
}

#[tokio::test]
async fn refresh_advances_the_issue_instant_past_the_previous_expiry() {
    let store = MemAccountStore::new();
    store.create_account(ACCOUNT, T0).await.expect("create");
    let node = node(1);
    store.bind(ACCOUNT, &node, T0).await.expect("bind");

    let ttl_ms = 60_000;
    let store = std::sync::Arc::new(store);
    let first = {
        let service = IdentityService::new(
            std::sync::Arc::clone(&store),
            StaticStanding::all_good(),
            FixedTokenClock::new(UnixMillis::new(T0)),
            IssuerKeyring::new(signing_key(1, 0xA1)),
        );
        service
            .issue(ACCOUNT, &node, Some(ttl_ms))
            .await
            .expect("issue")
    };
    assert_eq!(
        first.refresh_at_ms,
        UnixMillis::new(T0 + ttl_ms / 2),
        "docs/09 §8: refresh at half-TTL"
    );

    // The client comes back at half-TTL, as instructed.
    let service = IdentityService::new(
        std::sync::Arc::clone(&store),
        StaticStanding::all_good(),
        FixedTokenClock::new(first.refresh_at_ms),
        IssuerKeyring::new(signing_key(1, 0xA1)),
    );
    let second = service
        .refresh(first.claims(), Some(ttl_ms))
        .await
        .expect("refresh");

    assert!(
        second.claims().issued_at_ms > first.claims().issued_at_ms,
        "a refresh advances `issued_at_ms`"
    );

    // At an instant past the first token's expiry, the first is refused and the
    // second still verifies — which is the whole point of refreshing early.
    let after_first_expiry = SessionTokenVerifier::new(
        FixedTokenClock::new(UnixMillis::new(T0 + ttl_ms)),
        service.published_issuer_keys(),
    );
    assert_eq!(
        after_first_expiry.verify(&first.encoded, &node),
        Err(SessionTokenVerificationError::Expired)
    );
    assert!(after_first_expiry.verify(&second.encoded, &node).is_ok());
}

#[tokio::test]
async fn a_rotated_key_dual_accepts_and_then_stops_verifying() {
    let (service, node) = service().await;
    let old_key_id = service.active_issuer_key_id();
    let before = service.issue(ACCOUNT, &node, None).await.expect("issue");

    // Step 1+2: the new key is held and active; the old one is still published.
    service.rotate(signing_key(2, 0xB2)).expect("rotate");
    assert_ne!(service.active_issuer_key_id(), old_key_id);
    let after = service.issue(ACCOUNT, &node, None).await.expect("issue");
    assert_eq!(after.claims().issuer_key_id, service.active_issuer_key_id());

    // The dual-accept window: one verifier configured from the keyring accepts
    // tokens signed by either key.
    let dual = SessionTokenVerifier::new(
        FixedTokenClock::new(UnixMillis::new(T0 + 1)),
        service.published_issuer_keys(),
    );
    assert_eq!(service.published_issuer_keys().len(), 2);
    assert!(dual.verify(&before.encoded, &node).is_ok());
    assert!(dual.verify(&after.encoded, &node).is_ok());

    // Step 3: the window closes. The old key's tokens stop verifying, by name.
    service.retire_issuer_key(old_key_id).expect("retire");
    let closed = SessionTokenVerifier::new(
        FixedTokenClock::new(UnixMillis::new(T0 + 1)),
        service.published_issuer_keys(),
    );
    // The security property first, and the key count second as corroboration:
    // asserting the count first would let a broken `retire` fail on the count
    // and never reach the sentence that matters.
    assert_eq!(
        closed.verify(&before.encoded, &node),
        Err(SessionTokenVerificationError::UnknownIssuer(old_key_id)),
        "a retired key's tokens stop verifying, by name"
    );
    assert!(closed.verify(&after.encoded, &node).is_ok());
    assert_eq!(service.published_issuer_keys().len(), 1);
}

#[tokio::test]
async fn the_active_key_cannot_be_retired() {
    let (service, _node) = service().await;
    let active = service.active_issuer_key_id();
    assert_eq!(
        service.retire_issuer_key(active),
        Err(orrery_identity::RotationError::RetiringActiveKey(active)),
        "retiring the active key would leave nothing able to sign, and the \
         failure would surface at the next login rather than here"
    );
}

#[tokio::test]
async fn the_stamped_standing_is_the_standing_the_store_holds() {
    for standing in [SessionStanding::Good, SessionStanding::Quarantined] {
        let (service, node) = service_with_standing(standing).await;
        let session = service.issue(ACCOUNT, &node, None).await.expect("issue");
        let verifier = SessionTokenVerifier::new(
            FixedTokenClock::new(UnixMillis::new(T0 + 1)),
            service.published_issuer_keys(),
        );
        let claims = verifier.verify(&session.encoded, &node).expect("verify");
        assert_eq!(
            claims.standing, standing,
            "the field is read from the source, not hardcoded `Good`"
        );
    }
}

#[tokio::test]
async fn an_unresolvable_standing_refuses_to_mint() {
    let store = MemAccountStore::new();
    store.create_account(ACCOUNT, T0).await.expect("create");
    let node = node(1);
    store.bind(ACCOUNT, &node, T0).await.expect("bind");
    let service = IdentityService::new(
        store,
        UnavailableStanding,
        FixedTokenClock::new(UnixMillis::new(T0)),
        IssuerKeyring::new(signing_key(1, 0xA1)),
    );

    // D33 clause (f): a missing or unreadable ledger is never interpreted as
    // `Good`. The party able to make the lookup unavailable would otherwise
    // select the branch that admits a ban.
    assert_eq!(
        service
            .issue(ACCOUNT, &node, None)
            .await
            .expect_err("refused"),
        IdentityError::StandingUnavailable(ACCOUNT)
    );
}

#[tokio::test]
async fn minting_needs_an_account_and_a_current_binding() {
    let store = MemAccountStore::new();
    store.create_account(ACCOUNT, T0).await.expect("create");
    let bound = node(1);
    let unbound = node(2);
    store.bind(ACCOUNT, &bound, T0).await.expect("bind");
    let service = IdentityService::new(
        store,
        StaticStanding::all_good(),
        FixedTokenClock::new(UnixMillis::new(T0)),
        IssuerKeyring::new(signing_key(1, 0xA1)),
    );

    assert_eq!(
        service
            .issue(OTHER_ACCOUNT, &bound, None)
            .await
            .expect_err("no `da` row is an authentication failure"),
        IdentityError::UnknownAccount(OTHER_ACCOUNT)
    );
    assert_eq!(
        service
            .issue(ACCOUNT, &unbound, None)
            .await
            .expect_err("a token binds a node the account actually holds"),
        IdentityError::NotBound {
            node: unbound,
            account: ACCOUNT
        }
    );

    // And unbinding stops the refresh, immediately — the same four checks run
    // on a refresh as on a mint, which is what makes refresh able to enforce
    // anything at all.
    let session = service.issue(ACCOUNT, &bound, None).await.expect("issue");
    service
        .store()
        .unbind(ACCOUNT, &bound, T0 + 1)
        .await
        .expect("unbind");
    assert_eq!(
        service
            .refresh(session.claims(), None)
            .await
            .expect_err("refresh after unbinding is refused"),
        IdentityError::NotBound {
            node: bound,
            account: ACCOUNT
        }
    );
}

#[tokio::test]
async fn a_second_binding_leaves_the_first_and_unbinding_is_immediate() {
    let store = MemAccountStore::new();
    store.create_account(ACCOUNT, T0).await.expect("create");
    let first = node(1);
    let second = node(2);

    store.bind(ACCOUNT, &first, T0).await.expect("bind first");
    store
        .bind(ACCOUNT, &second, T0 + 1)
        .await
        .expect("bind second");

    let row = store.account(ACCOUNT).await.expect("read").expect("row");
    assert_eq!(row.bound_nodes, vec![first, second]);
    // The account row's epoch and the write-time fold both move on every event.
    assert_eq!(row.binding_epoch, 2);
    assert_eq!(row.binding_event_count, 2);
    assert_eq!(row.first_event_ms, T0);

    // `owner(n)` answers for both, through the trait the gateway calls.
    assert_eq!(store.owner(&first), Some(ACCOUNT));
    assert_eq!(store.owner(&second), Some(ACCOUNT));

    store
        .unbind(ACCOUNT, &first, T0 + 2)
        .await
        .expect("unbind first");
    assert_eq!(
        store.owner(&first),
        None,
        "docs/09 §8: unbinding is immediate, and the released NodeId's lookup \
         becomes a miss — which excludes"
    );
    assert_eq!(store.owner(&second), Some(ACCOUNT), "the other stands");
    assert_eq!(
        store
            .account(ACCOUNT)
            .await
            .expect("read")
            .expect("row")
            .bound_nodes,
        vec![second]
    );

    // The history keeps every event, in order, keyed by node.
    let history = store.binding_history(&first).await.expect("history");
    assert_eq!(history.len(), 2);
    assert_eq!(
        history.iter().map(|row| row.at_ms).collect::<Vec<_>>(),
        vec![T0, T0 + 2]
    );
}

#[tokio::test]
async fn a_node_binds_to_at_most_one_account() {
    let store = MemAccountStore::new();
    store.create_account(ACCOUNT, T0).await.expect("create");
    store
        .create_account(OTHER_ACCOUNT, T0)
        .await
        .expect("create");
    let node = node(1);
    store.bind(ACCOUNT, &node, T0).await.expect("bind");

    assert_eq!(
        store
            .bind(OTHER_ACCOUNT, &node, T0 + 1)
            .await
            .expect_err("a bound node is not silently re-pointed"),
        IdentityError::NodeBoundElsewhere {
            node,
            account: ACCOUNT
        }
    );
    // The refusal left nothing behind: `owner(n)` still answers the first
    // account, and no event was folded into either row.
    assert_eq!(store.owner(&node), Some(ACCOUNT));
    assert_eq!(
        store
            .account(OTHER_ACCOUNT)
            .await
            .expect("read")
            .expect("row")
            .binding_event_count,
        0
    );
}

#[tokio::test]
async fn the_store_feeds_the_gateways_snapshot_authority() {
    let store = MemAccountStore::new();
    store.create_account(ACCOUNT, T0).await.expect("create");
    store
        .create_account(OTHER_ACCOUNT, T0)
        .await
        .expect("create");
    let mine = node(1);
    let theirs = node(2);
    let stranger = node(3);
    store.bind(ACCOUNT, &mine, T0).await.expect("bind");
    store.bind(OTHER_ACCOUNT, &theirs, T0).await.expect("bind");

    let authority =
        orrery_persistd::gateway::SnapshotBindingAuthority::from_bindings(store.bindings());
    assert_eq!(authority.owner(&mine), Some(ACCOUNT));
    assert_eq!(authority.owner(&theirs), Some(OTHER_ACCOUNT));
    assert_eq!(
        authority.owner(&stranger),
        None,
        "an unbound node is unresolved, and D31 clause (f) reads that as \
         `exclude`, never as `not a party`"
    );
}
