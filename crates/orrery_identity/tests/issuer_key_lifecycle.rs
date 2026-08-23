//! D41(d)'s issuer-key recovery ceremony, including the production verifier.

use age::secrecy::SecretString;
use orrery_identity::{
    escrow_issuer_key, generate_issuer_key, load_issuer_key, load_runtime_credential,
    restore_issuer_key, write_runtime_credential, IssuerKeyLifecycleError, IssuerKeyring,
    IssuerSigningKey,
};
use orrery_protocol::{
    AccountId, FixedTokenClock, IssuerKeyId, NodeId, SessionStanding, SessionTokenClaimsV1,
    SessionTokenTtlMs, SessionTokenVerificationError, SessionTokenVerifier, UnixMillis,
};
use std::fs;
use std::path::Path;

const T0: u64 = 1_700_000_000_000;

fn passphrase(value: &str) -> SecretString {
    SecretString::from(value.to_owned())
}

fn staging_key(directory: &Path) -> (IssuerSigningKey, std::path::PathBuf) {
    let key = generate_issuer_key(IssuerKeyId::new(371));
    let path = directory.join("issuer.runtime");
    write_runtime_credential(&path, &key).expect("write staging credential");
    (key, path)
}

fn signed_rehearsal_token(key: IssuerSigningKey, node: NodeId) -> Vec<u8> {
    let keyring = IssuerKeyring::new(key);
    keyring
        .sign(SessionTokenClaimsV1 {
            version: 2,
            account: AccountId(371),
            node,
            issued_at_ms: UnixMillis::new(T0),
            ttl_ms: SessionTokenTtlMs::new(60_000),
            standing: SessionStanding::Good,
            issuer_key_id: keyring.active_key_id(),
            on_probation: false,
        })
        .expect("restored key signs")
        .encode()
        .expect("encode signed token")
}

fn verify_rehearsal_token(
    encoded: &[u8],
    node: &NodeId,
    issuer: orrery_protocol::IssuerKey,
) -> Result<orrery_protocol::SessionTokenClaimsV1, SessionTokenVerificationError> {
    // This is the exact verifier instantiated by coordinators and gateways.
    SessionTokenVerifier::new(FixedTokenClock::new(UnixMillis::new(T0 + 1)), [issuer])
        .verify(encoded, node)
}

#[test]
fn restored_key_signs_token_accepted_by_existing_session_token_verifier() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (generated, staging) = staging_key(directory.path());
    let expected_public_key = generated.public_key();
    let escrow = directory.path().join("issuer.age");
    escrow_issuer_key(
        &staging,
        &escrow,
        passphrase("correct horse battery staple unique to this escrow"),
    )
    .expect("escrow generated key");
    let restored = restore_issuer_key(
        &escrow,
        passphrase("correct horse battery staple unique to this escrow"),
        expected_public_key,
    )
    .expect("restore and compare public key");

    assert_eq!(restored.public_key(), expected_public_key);
    let node = generate_issuer_key(IssuerKeyId::new(999)).public_key();
    let encoded = signed_rehearsal_token(restored.clone(), node);
    let claims = verify_rehearsal_token(&encoded, &node, restored.issuer_key())
        .expect("the existing verifier accepts the restored key's token");
    assert_eq!(claims.account, AccountId(371));

    // This verifier-specific behavior prevents a decode-only or bespoke
    // signature check from standing in for SessionTokenVerifier in the test.
    let other_node = generate_issuer_key(IssuerKeyId::new(1000)).public_key();
    assert_eq!(
        verify_rehearsal_token(&encoded, &other_node, restored.issuer_key()),
        Err(SessionTokenVerificationError::WrongNode)
    );
}

#[test]
fn corrupted_escrow_ciphertext_fails_closed() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (generated, staging) = staging_key(directory.path());
    let escrow = directory.path().join("issuer.age");
    escrow_issuer_key(
        &staging,
        &escrow,
        passphrase("one strong unique passphrase"),
    )
    .expect("escrow");
    let mut ciphertext = fs::read(&escrow).expect("read escrow");
    let last = ciphertext.last_mut().expect("non-empty age file");
    *last ^= 0x80;
    fs::write(&escrow, ciphertext).expect("corrupt ciphertext stage");

    assert!(matches!(
        restore_issuer_key(
            &escrow,
            passphrase("one strong unique passphrase"),
            generated.public_key()
        ),
        Err(IssuerKeyLifecycleError::Decrypt(_))
    ));
}

#[test]
fn wrong_passphrase_fails_closed() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (generated, staging) = staging_key(directory.path());
    let escrow = directory.path().join("issuer.age");
    escrow_issuer_key(
        &staging,
        &escrow,
        passphrase("the actual strong passphrase"),
    )
    .expect("escrow");

    assert!(matches!(
        restore_issuer_key(
            &escrow,
            passphrase("a different strong passphrase"),
            generated.public_key()
        ),
        Err(IssuerKeyLifecycleError::Decrypt(_))
    ));
}

#[test]
fn restore_refuses_public_key_mismatch() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (_generated, staging) = staging_key(directory.path());
    let escrow = directory.path().join("issuer.age");
    escrow_issuer_key(
        &staging,
        &escrow,
        passphrase("another strong unique passphrase"),
    )
    .expect("escrow");
    let different = generate_issuer_key(IssuerKeyId::new(372));

    assert!(matches!(
        restore_issuer_key(
            &escrow,
            passphrase("another strong unique passphrase"),
            different.public_key()
        ),
        Err(IssuerKeyLifecycleError::PublicKeyMismatch { .. })
    ));
}

#[test]
fn load_writes_the_same_restrictive_runtime_credential() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (generated, staging) = staging_key(directory.path());
    let escrow = directory.path().join("issuer.age");
    let runtime = directory.path().join("loaded.runtime");
    escrow_issuer_key(&staging, &escrow, passphrase("boot-only strong passphrase"))
        .expect("escrow");
    let loaded = load_issuer_key(
        &escrow,
        &runtime,
        passphrase("boot-only strong passphrase"),
        generated.public_key(),
    )
    .expect("load");
    assert_eq!(loaded.public_key(), generated.public_key());
    let service_key = load_runtime_credential(&runtime).expect("identity service reads credential");
    let service_keyring = IssuerKeyring::new(service_key);
    assert_eq!(
        service_keyring.published_keys(),
        vec![generated.issuer_key()]
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&runtime)
                .expect("runtime metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&escrow)
                .expect("escrow metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn signing_key_debug_never_contains_secret_bytes() {
    let key = generate_issuer_key(IssuerKeyId::new(373));
    let debug = format!("{key:?}");
    assert!(debug.contains("public_key"));
    assert!(!debug.contains("secret"));
}

#[test]
fn secret_bearing_outputs_are_refused_inside_a_repository() {
    let key = generate_issuer_key(IssuerKeyId::new(374));
    let output = Path::new(env!("CARGO_MANIFEST_DIR")).join("must-not-exist.runtime");
    assert!(matches!(
        write_runtime_credential(&output, &key),
        Err(IssuerKeyLifecycleError::RepositoryPath(_))
    ));
    assert!(!output.exists());
}
