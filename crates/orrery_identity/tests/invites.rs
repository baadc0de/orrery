//! Invite redemption through the existing token verifier.

use orrery_identity::AccountStore;
use orrery_identity::{
    mint_invite, redeem_invite, IdentityService, InviteCodeGenerator, InviteLedger,
    InviteRedemptionError, IssuerKeyring, IssuerSigningKey, MemAccountStore, StaticStanding,
};
use orrery_protocol::{FixedTokenClock, IssuerKeyId, NodeId, SessionTokenVerifier, UnixMillis};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

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

struct TemporaryLedger {
    directory: PathBuf,
    path: PathBuf,
}

impl Drop for TemporaryLedger {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn ledger_path() -> TemporaryLedger {
    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);
    let directory = std::env::temp_dir().join(format!(
        "orrery-identity-invites-{}-{}",
        std::process::id(),
        NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed),
    ));
    fs::create_dir(&directory).expect("temporary ledger directory");
    TemporaryLedger {
        path: directory.join("invites.tsv"),
        directory,
    }
}

fn mint(path: &Path, byte: u8) -> orrery_identity::MintedInvite {
    InviteLedger::update_locked(path, |ledger| {
        mint_invite(
            ledger,
            "Ada".to_owned(),
            &mut FixedCode([byte; 32]),
            UnixMillis::new(T0),
        )
    })
    .expect("mint an offline invite")
}

#[test]
fn concurrent_mints_keep_both_allocations() {
    let ledger = ledger_path();
    let barrier = Arc::new(Barrier::new(2));
    let mut mints = Vec::new();
    for byte in [10, 11] {
        let path = ledger.path.clone();
        let barrier = Arc::clone(&barrier);
        mints.push(std::thread::spawn(move || {
            barrier.wait();
            mint(&path, byte).account
        }));
    }
    let mut accounts = mints
        .into_iter()
        .map(|mint| mint.join().expect("mint thread"))
        .collect::<Vec<_>>();
    accounts.sort_by_key(|account| account.0);
    assert_eq!(
        accounts,
        vec![orrery_protocol::AccountId(1), orrery_protocol::AccountId(2)]
    );
    assert_eq!(
        mint(&ledger.path, 12).account,
        orrery_protocol::AccountId(3)
    );
}

#[tokio::test]
async fn invite_redemption_round_trips_through_the_existing_token_verifier() {
    let ledger = ledger_path();
    let minted = mint(&ledger.path, 7);
    let identity = service();
    let node = node(3);

    let session = redeem_invite(
        &ledger.path,
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
    let ledger = ledger_path();
    let minted = mint(&ledger.path, 8);
    let identity = service();
    let node = node(4);

    assert!(matches!(
        redeem_invite(
            &ledger.path,
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

#[tokio::test]
async fn consumed_invite_is_refused_before_it_touches_the_account_store() {
    let ledger = ledger_path();
    let minted = mint(&ledger.path, 9);
    let identity = service();
    let first_node = node(5);

    redeem_invite(
        &ledger.path,
        &minted.code,
        &first_node,
        UnixMillis::new(T0),
        &identity,
        None,
    )
    .await
    .expect("first redemption consumes the invite");
    let accounts_before = identity.store().accounts();
    let bindings_before = identity.store().bindings();

    assert!(matches!(
        redeem_invite(
            &ledger.path,
            &minted.code,
            &node(6),
            UnixMillis::new(T0 + 1),
            &identity,
            None,
        )
        .await,
        Err(InviteRedemptionError::AlreadyConsumed)
    ));
    assert_eq!(identity.store().accounts(), accounts_before);
    assert_eq!(identity.store().bindings(), bindings_before);
}
