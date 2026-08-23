//! Invite redemption through the existing token verifier.

use orrery_identity::AccountStore;
use orrery_identity::{
    mint_invite, redeem_invite, IdentityService, InviteCodeGenerator, InviteLedger,
    InviteRedemptionError, IssuerKeyring, IssuerSigningKey, MemAccountStore, StaticStanding,
};
use orrery_protocol::{FixedTokenClock, IssuerKeyId, NodeId, SessionTokenVerifier, UnixMillis};

const T0: u64 = 1_700_000_000_000;

#[derive(Debug)]
struct FixedCode([u8; 32]);

impl InviteCodeGenerator for FixedCode {
    fn generate_code_bytes(&mut self) -> [u8; 32] {
        self.0
    }
}

fn secret(seed: u8) -> iroh_base::SecretKey {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    bytes[31] = 0x11;
    iroh_base::SecretKey::from_bytes(&bytes)
}

fn node(seed: u8) -> NodeId {
    secret(seed).public()
}

fn service() -> IdentityService<MemAccountStore, StaticStanding, FixedTokenClock> {
    IdentityService::new(
        MemAccountStore::new(),
        StaticStanding::all_good(),
        FixedTokenClock::new(UnixMillis::new(T0)),
        IssuerKeyring::new(IssuerSigningKey::new(IssuerKeyId::new(41), secret(91))),
    )
}

#[tokio::test]
async fn invite_redemption_round_trips_through_the_existing_token_verifier() {
    let mut ledger = InviteLedger::default();
    let minted = mint_invite(&mut ledger, "Ada".to_owned(), &mut FixedCode([7; 32]))
        .expect("mint an offline invite");
    let identity = service();
    let node = node(3);

    let session = redeem_invite(
        &ledger,
        &minted.code,
        &node,
        UnixMillis::new(T0),
        &identity,
        Some(60_000),
    )
    .await
    .expect("redeem");

    // This is the protocol verifier the coordinator already instantiates for
    // admission; redemption does not introduce a second verifier.
    let verifier = SessionTokenVerifier::new(
        FixedTokenClock::new(UnixMillis::new(T0 + 1)),
        identity.published_issuer_keys(),
    );
    let claims = verifier
        .verify(&session.encoded, &node)
        .expect("existing verifier accepts redeemed token");
    assert_eq!(claims.account, minted.account);
    assert_eq!(claims.node, node);
}

#[tokio::test]
async fn wrong_code_is_refused_before_it_creates_or_binds_an_account() {
    let mut ledger = InviteLedger::default();
    let minted = mint_invite(&mut ledger, "Ada".to_owned(), &mut FixedCode([8; 32]))
        .expect("mint an offline invite");
    let identity = service();
    let node = node(4);

    assert!(matches!(
        redeem_invite(
            &ledger,
            "orrery-invite-v1-not-the-issued-code",
            &node,
            UnixMillis::new(T0),
            &identity,
            None,
        )
        .await,
        Err(InviteRedemptionError::InvalidCode)
    ));
    assert!(
        identity
            .store()
            .account(minted.account)
            .await
            .expect("read store")
            .is_none(),
        "the invalid-code guard runs before account creation"
    );
    // Checking only `minted.account` would pass for a verify-after-mutate bug
    // that writes some *other* id: the store gets polluted on every wrong code
    // and the assertion above looks the other way. The function's contract is
    // that a bad code touches the store at all, so assert on the whole binding
    // set rather than on the one row we happen to know the id of.
    assert!(
        identity.store().bindings().is_empty(),
        "a refused code must leave no binding behind, whatever account it names"
    );
    assert!(
        identity.store().accounts().is_empty(),
        "a refused code must create no account row, whatever id it names"
    );
}
