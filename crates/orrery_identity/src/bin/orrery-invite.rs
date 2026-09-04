//! Offline invite-code minting and offline session-token signing.
//!
//! This binary is where campaign invites are minted (#387): on the operator's
//! own machine, against a local hash-only ledger, with no network, no minting
//! service and no webpage. `mint` allocates the account, the invite code and
//! the **pre-minted campaign session id** — the UUIDv7 a banked human hour
//! carries as `identity.human_session_id`, unique under this ledger.
//!
//! `session-token` signs a `SessionTokenClaimsV1` for one session, still
//! offline, against the D41 issuer credential. It is a separate step from
//! `mint` because `MAX_SESSION_TOKEN_TTL_MS` is one hour: a token signed at
//! invite time would be expired before the session it was minted for, so the
//! operator signs it shortly before hosting and the host verifies it at join
//! (#345 §8).

use clap::{Parser, Subcommand};
use orrery_identity::{
    load_runtime_credential, mint_invite, InviteLedger, IssuerKeyring, OsInviteCodeGenerator,
};
use orrery_protocol::{
    AccountId, CampaignJoinFileV1, NodeId, SessionStanding, SessionTokenClaimsV1,
    SessionTokenTtlMs, TokenClock as _, MAX_SESSION_TOKEN_TTL_MS,
};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Offline campaign identity material: invites and session tokens.
#[derive(Debug, Parser)]
#[command(name = "orrery-invite")]
struct Arguments {
    #[command(subcommand)]
    operation: Operation,
}

#[derive(Debug, Subcommand)]
enum Operation {
    /// Mint one invite code into a local hash-only ledger.
    Mint {
        /// Local tab-separated invite ledger; created if it does not exist.
        #[arg(long, env = "ORRERY_LEDGER")]
        ledger: PathBuf,
        /// Operator's volunteer label, stored alongside the allocated account id.
        #[arg(long, env = "ORRERY_LABEL")]
        label: String,
    },
    /// Sign one session token, offline, against the issuer credential.
    ///
    /// This is the one mint path that stamps a constant
    /// [`SessionStanding::Good`], and it does so on purpose (#861): there is
    /// no standing ledger on an operator's offline laptop for D33 clause (f)'s
    /// fail-closed read to consult, so the `Good` is the operator's own
    /// attestation — the volunteer was handed the invite by name and is being
    /// hosted by hand. It is exactly the posture the issue's acceptance
    /// evidence exempts: the *served* mint path, `orrery-identity`, is the one
    /// that must read real standing, and it does; this path exists so
    /// shakedown hosting does not depend on that service being up. A token
    /// signed here is indistinguishable on the wire from one minted there,
    /// which is why the exemption is stated here rather than left implied.
    ///
    /// What #1014 changed is only that the attestation is now *stated*:
    /// `--assume-standing-good` is mandatory, so the operator cannot reach
    /// the constant without naming it. Nothing about the token moved; see
    /// [`offline_standing`] for what the flag is acknowledging.
    SessionToken {
        /// Plain runtime credential from `orrery-issuer-key generate`/`load`;
        /// must be owner-readable only and outside every repository.
        #[arg(long, env = "ORRERY_ISSUER_CREDENTIAL")]
        issuer_credential: PathBuf,
        /// The account the invite allocated (`mint` printed it).
        #[arg(long, env = "ORRERY_ACCOUNT")]
        account: u64,
        /// The transport identity the token authorizes — for a campaign
        /// external slot, the persistent key the client prints
        /// (`orrery-regolith --print-slot-key <n>`).
        #[arg(long, env = "ORRERY_NODE")]
        node: String,
        /// Requested lifetime; the protocol caps it at one hour.
        #[arg(long, default_value_t = MAX_SESSION_TOKEN_TTL_MS, env = "ORRERY_TTL_MS")]
        ttl_ms: u64,
        /// Write a complete, named-field join file instead of requiring the
        /// volunteer to transcribe its launch material.
        #[arg(long, env = "ORRERY_JOIN_FILE")]
        join_file: Option<PathBuf>,
        /// Hosting process NodeId included in `--join-file`.
        #[arg(long, env = "ORRERY_HOST_NODE")]
        host_node: Option<String>,
        /// Client slot included in `--join-file`; the token is independently
        /// bound to the persistent transport identity supplied by `--node`.
        #[arg(long, env = "ORRERY_SLOT")]
        slot: Option<usize>,
        /// Pre-minted session ID included in `--join-file`.
        #[arg(long, env = "ORRERY_SESSION_ID")]
        session_id: Option<String>,
        /// Required. Attest that this account's standing is `Good` on the
        /// operator's own authority, because this path reads no ledger.
        ///
        /// It is not a formality: what it skips is the whole ladder. A
        /// **quarantined** account is stamped `Good` and so skips D10's full
        /// cluster-side write validation; a **cooled-down** or **banned**
        /// account — which the served path refuses to mint for at all — is
        /// handed a working token. See [`offline_standing`].
        #[arg(long, env = "ORRERY_ASSUME_STANDING_GOOD")]
        assume_standing_good: bool,
    },
}

/// The standing an offline mint may stamp, given the operator's attestation.
///
/// # What the flag acknowledges
///
/// There is no standing ledger on an offline laptop, so `Good` here is a
/// claim the operator makes, not a value anything read. Refusing without
/// `--assume-standing-good` is what stops it from being a *silent* claim
/// (#1014); the behaviour with the flag is byte-identical to what this
/// binary always did.
///
/// The reason the acknowledgement has to be explicit is that **nothing
/// downstream re-derives it**. Both live enforcement points compare an
/// invalidation watermark against the token's signed `issued_at_ms` —
/// `StandingState::verdict` in `orrery_coordinator::server` for `Hello`, and
/// `AccountStandings::pending` in `orrery_protocol::standing` for the
/// gateway's admission and sweep — and both let the *token* win when it
/// postdates the assertion. A token signed now postdates every assertion
/// filed before now, by construction. So the value stamped here is the value
/// the session runs on.
///
/// The one thing the constant does not buy is a witness seat: this path also
/// stamps `on_probation: true`, and D28 clause (e)'s `eligible_pool` excludes
/// a probationary session unconditionally.
fn offline_standing(attested: bool) -> Result<SessionStanding, &'static str> {
    if attested {
        Ok(SessionStanding::Good)
    } else {
        Err(OFFLINE_STANDING_REFUSAL)
    }
}

/// Why an unattested offline mint is refused, and the two ways forward.
const OFFLINE_STANDING_REFUSAL: &str = concat!(
    "refusing to mint: this offline path consults no standing ledger, and would ",
    "stamp SessionStanding::Good without one. A quarantined account would skip ",
    "D10 full cluster-side write validation; a cooled-down or banned account, ",
    "which the served mint path refuses outright, would get a working token. ",
    "Nothing downstream catches either: a freshly signed token postdates every ",
    "invalidation watermark, so the token wins. Mint through the served path ",
    "(orrery-identity) if the cluster is reachable, or pass ",
    "--assume-standing-good to attest this account's standing yourself.",
);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    match Arguments::parse().operation {
        Operation::Mint { ledger, label } => {
            let now = orrery_identity::SystemClock.now_ms();
            let minted = InviteLedger::update_locked(&ledger, |ledger| {
                mint_invite(ledger, label, &mut OsInviteCodeGenerator, now)
            })?;
            println!("account={}", minted.account.0);
            println!("invite_code={}", minted.code);
            println!("session_id={}", minted.session);
        }
        Operation::SessionToken {
            issuer_credential,
            account,
            node,
            ttl_ms,
            join_file,
            host_node,
            slot,
            session_id,
            assume_standing_good,
        } => {
            // Before the credential is even opened: a refusal must not be the
            // thing an operator discovers after unlocking the issuer key.
            let standing = offline_standing(assume_standing_good)?;
            let key = load_runtime_credential(&issuer_credential)?;
            let node = NodeId::from_str(&node)?;
            let keyring = IssuerKeyring::new(key);
            let claims = SessionTokenClaimsV1::new(
                AccountId(account),
                node,
                orrery_identity::SystemClock.now_ms(),
                SessionTokenTtlMs(ttl_ms.min(MAX_SESSION_TOKEN_TTL_MS)),
                standing,
                keyring.active_key_id(),
                // A shakedown volunteer is inside D33 clause (d)'s probation
                // window by construction; the closed direction is the truth.
                true,
            );
            let token = keyring.sign(claims)?;
            let token_hex = hex(&token.encode()?);
            if let Some(path) = join_file {
                let host_node = host_node.ok_or("--join-file needs --host-node <NodeId>")?;
                let slot = slot.ok_or("--join-file needs --slot <n>")?;
                let session_id = session_id.ok_or("--join-file needs --session-id <UUID>")?;
                write_join_file(
                    &path,
                    &CampaignJoinFileV1::new(host_node, slot, session_id, token_hex),
                )?;
                println!("join_file={}", path.display());
            } else {
                // Compatibility path: existing automation uses this argv-ready
                // value. Prefer --join-file or --session-token @path for new
                // sessions, because argv exposes the token to shell history and ps.
                println!("session_token={token_hex}");
            }
            println!("issuer_key_id={}", keyring.active_key_id().0);
            println!(
                "issuer_public_key={}",
                keyring.published_keys()[0].public_key
            );
        }
    }
    Ok(())
}

/// Write one new owner-readable join file without replacing an existing secret.
fn write_join_file(
    path: &Path,
    join: &CampaignJoinFileV1,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(join.to_json()?.as_bytes())?;
    Ok(())
}

/// Lowercase hex, so the token can travel through a flag or an env var.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{hex, offline_standing, write_join_file, OFFLINE_STANDING_REFUSAL};
    use orrery_identity::{generate_issuer_key, IssuerKeyring};
    use orrery_protocol::{
        AccountId, CampaignJoinFileV1, FixedTokenClock, IssuerKeyId, SessionStanding,
        SessionTokenClaimsV1, SessionTokenTtlMs, SessionTokenVerificationError,
        SessionTokenVerifier, UnixMillis,
    };

    #[test]
    fn signed_token_join_file_round_trips_through_the_shared_client_format() {
        let keyring = IssuerKeyring::new(generate_issuer_key(IssuerKeyId::new(47)));
        let client_node = iroh_base::SecretKey::from_bytes(&[7; 32]).public();
        let token = keyring
            .sign(SessionTokenClaimsV1::new(
                AccountId::new(8),
                client_node,
                UnixMillis::new(1_700_000_000_000),
                SessionTokenTtlMs(60_000),
                SessionStanding::Good,
                keyring.active_key_id(),
                true,
            ))
            .expect("sign token");
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("volunteer.join.json");
        let expected = CampaignJoinFileV1::new(
            "host-node".to_owned(),
            3,
            "018f0000-0000-7000-8000-000000000047".to_owned(),
            hex(&token.encode().expect("encode token")),
        );

        write_join_file(&path, &expected).expect("emit join file");
        let consumed = CampaignJoinFileV1::from_json(
            &std::fs::read_to_string(&path).expect("read emitted join file"),
        )
        .expect("client join format accepts emitted file");
        assert_eq!(consumed, expected);
        let verifier = SessionTokenVerifier::new(
            FixedTokenClock::new(UnixMillis::new(1_700_000_000_001)),
            keyring.published_keys(),
        );
        assert_eq!(
            consumed.session_token,
            hex(&token.encode().expect("encode signed token")),
            "the emitted file preserves the signed token bytes"
        );
        let encoded = token.encode().expect("encode signed token");
        assert!(verifier.verify(&encoded, &client_node).is_ok());
        assert_eq!(
            verifier.verify(
                &encoded,
                &iroh_base::SecretKey::from_bytes(&[8; 32]).public()
            ),
            Err(SessionTokenVerificationError::WrongNode),
            "the join file carries the same token; it cannot authorize another transport key"
        );
    }

    /// #1014: the constant `Good` is still available, but only to an operator
    /// who has said so. Without the flag the mint stops, and it stops with a
    /// message naming each rung of the ladder it would otherwise have skipped
    /// — because "standing was not checked" is exactly the sentence the
    /// operator with a legitimate reason to be here would not otherwise read.
    #[test]
    fn an_unattested_offline_mint_is_refused_and_the_refusal_names_what_it_skips() {
        let refusal = offline_standing(false).expect_err("no attestation, no token");
        assert_eq!(refusal, OFFLINE_STANDING_REFUSAL);
        for rung in ["quarantined", "cooled-down", "banned"] {
            assert!(
                refusal.contains(rung),
                "the refusal must name the {rung} case it would have waved through"
            );
        }
        assert!(
            refusal.contains("--assume-standing-good"),
            "a refusal that does not name its own escape hatch is a dead end"
        );
    }

    /// The legitimate offline workflow is unchanged: with the attestation, the
    /// path signs exactly the token it always signed, and a verifier holding
    /// the issuer's published keys accepts it for the bound transport identity.
    /// This is the shakedown-hosting case the subcommand exists for, and it
    /// must keep working with no cluster in reach.
    #[test]
    fn an_attested_offline_mint_signs_the_same_token_it_always_did() {
        let standing = offline_standing(true).expect("the attestation is the whole gate");
        assert_eq!(standing, SessionStanding::Good);

        let keyring = IssuerKeyring::new(generate_issuer_key(IssuerKeyId::new(11)));
        let client_node = iroh_base::SecretKey::from_bytes(&[9; 32]).public();
        let issued_at = UnixMillis::new(1_700_000_000_000);
        let token = keyring
            .sign(SessionTokenClaimsV1::new(
                AccountId::new(4),
                client_node,
                issued_at,
                SessionTokenTtlMs(60_000),
                standing,
                keyring.active_key_id(),
                true,
            ))
            .expect("sign token");
        let encoded = token.encode().expect("encode signed token");

        let verifier = SessionTokenVerifier::new(
            FixedTokenClock::new(UnixMillis::new(1_700_000_000_001)),
            keyring.published_keys(),
        );
        let claims = verifier
            .verify(&encoded, &client_node)
            .expect("an offline-minted token still admits its volunteer");
        assert_eq!(claims.standing, SessionStanding::Good);
        assert!(
            claims.on_probation,
            "the offline path still forfeits witness eligibility (D28 clause (e))"
        );
    }
}
