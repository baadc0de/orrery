//! D32 clause (i)'s authenticated posture row: the operator's signature travels
//! *in* `ramp/{control}` and every `persistd` verifies it before the mode may
//! take effect
//! ([D32](../../../../docs/adr/0032-enforcement-ramp.md) clause (i)).
//!
//! # Why the check is here and not at a writer
//!
//! Clause (c) makes `ramp/{control}` the single runtime lever for every
//! enforcement control, so whatever authenticates this row is the whole of the
//! authorisation story for runtime enforcement control. Open question 1 named
//! two candidates and the spike behind
//! [#932](https://github.com/baadc0de/orrery/pull/932) measured three, because
//! the second splits:
//!
//! | | Where the check runs | Authority to command the fleet |
//! |---|---|---|
//! | direct FDB write by an ops tool | nowhere | possession of the cluster file |
//! | envelope verified at **write** time | in the writer service | the cluster file **or** an operator key |
//! | signed row verified at **read** time | in every `persistd` | an operator key only |
//!
//! The middle row is the one that looks safe and is not. A signed envelope
//! verified by a privileged writer authenticates *the API*; the stored row is
//! still a plain byte string, so anybody who can reach FoundationDB writes it
//! directly and the fleet cannot tell the difference. The spike demonstrated
//! exactly that against a live cluster — verify the envelope, then write the
//! row by hand, then watch the mode change land anyway.
//!
//! This module implements the third row. Because the check is on the read side,
//! a raw FoundationDB write is not a bypass: it is a row that every gateway
//! refuses.
//!
//! # The preimage, and why both of its first two fields are load-bearing
//!
//! ```text
//! preimage = blake3("orrery/d32/ramp-posture/v1\0"
//!                 ‖ u32le(len(control)) ‖ control
//!                 ‖ u8(mode) ‖ u8(source) ‖ u64le(set_at_ms)
//!                 ‖ u32le(len(reason)) ‖ reason
//!                 ‖ opt(incident_id) ‖ opt(expires_at_ms))
//! ```
//!
//! The **domain-separation constant is first** so a posture signature can never
//! be replayed as any other Orrery signature, and so no other Orrery signature
//! can be replayed as a posture. The **control name is bound second** so a
//! signature for one control cannot be moved to another's key: without it, a
//! legitimately signed `ramp/authority_correction = off` row could be copied to
//! `ramp/strikes` by anyone with write access — a valid signature authorising a
//! posture nobody authorised. Both have a failing-if-removed test below
//! ([`a_signature_does_not_transfer_between_controls`],
//! [`the_domain_constant_is_not_decoration`]).
//!
//! The signature is over the 32-byte digest rather than over a postcard
//! encoding of the row, deliberately: the row's postcard encoding is a
//! *storage* format that clause (i)'s schema tag is expected to revise, and a
//! preimage that moves when the storage format moves is a preimage that
//! silently invalidates every previously signed row.
//!
//! # The migration hazard this module's tag exists for
//!
//! postcard is positional and prefix-tolerant. Appending the authenticator to
//! [`RampPosture`] produces bytes the *pre-amendment*
//! `FdbRampPostureStore::read` decodes **successfully**, dropping the signature
//! on the floor and applying the mode. A rolling upgrade would therefore leave
//! un-upgraded gateways obeying rows they never authenticated, while the
//! mechanism appeared to be deployed. So the value is tagged per
//! [D38](../../../../docs/adr/0038-at-rest-schema-versioning.md) with a leading
//! byte the old reader *refuses* — see [`RAMP_POSTURE_SCHEMA`] for why the
//! first tagged schema number is 3 and not 1, and
//! [`the_pre_amendment_reader_refuses_a_tagged_value`] for the proof.
//!
//! # How this composes with #876's admission predicates
//!
//! Three checks, each strictly narrower than the last, each refusing
//! independently, and each living where its inputs live:
//!
//! | Check | Question | Where |
//! |---|---|---|
//! | [`admit`] | who wrote this? | this module, from the store, over the signed bytes |
//! | [`RampPosture::admissible`] | was anyone allowed to write this at all? | #876, row-local |
//! | [`RampPosture::admissible_from`] | is this a legal transition from what we are doing now? | #876, at the poller |
//!
//! `admit` **calls** `admissible` rather than restating clause (f)'s rule, and
//! it never relaxes it. The operator arm and the automation arm are disjoint —
//! an `AutoSuspend` row is unsigned by design — so the composition has no
//! precedence question to get wrong.
//!
//! # Which controls this lever actually reaches
//!
//! Clause (i) puts the check in [`super::ramp::FdbRampPostureStore::read`], so
//! a control is levered exactly when something polls that store for it. As this
//! lands:
//!
//! | | Control | Poller | Note |
//! |---|---|---|---|
//! | C1 | `attestation_quorum` | yes ([#863](https://github.com/baadc0de/orrery/issues/863)) | |
//! | C2 | `quarantine_validation` | **no** | see below |
//! | C3 | write refusal / annulment | **no** | has no posture type at all — D32 clause (c) gives it a flag, and nothing in the tree carries a C3 mode, so there is no cell for a poller to refresh |
//! | C4 | `authority_correction` | yes | `spawn_authority_correction_poller` |
//! | C5 | `strikes` | yes ([#863](https://github.com/baadc0de/orrery/issues/863)) | two cells, gateway and coordinator |
//!
//! **C2's absence is a consequence of closing open question 3, not an
//! oversight.** Its `off` arm does not exist, so the only row it could ever
//! accept is `shadow` — and C2 has no shadow arm implemented: its check is
//! unconditional on the intent path. A poller for a cell with one reachable
//! value would refresh nothing. Building C2's shadow arm is the *control's*
//! half of #863, not the lever's, and until it exists this verifier is what
//! stands between C2 and a demotion nobody authorised: it refuses `off`
//! outright and refuses `shadow` without an expiry.
//!
//! # What this module does not decide
//!
//! Operator key custody, issuance and rotation are
//! [D41](../../../../docs/adr/0041-offline-identity-issuer-custody-and-lifecycle.md)'s.
//! This module consumes a verifying-key set and invents no custody scheme.

use orrery_protocol::{NodeId, Signature};
use serde::{Deserialize, Serialize};

use super::ramp::{PostureSource, RampMode, RampPosture};

/// Domain separator for a `ramp/{control}` posture signature.
///
/// First in the preimage, and trailing NUL, matching every other Orrery domain
/// constant (`AUTHORITY_CORRECTION_V1_DOMAIN` and friends).
pub const RAMP_POSTURE_V1_DOMAIN: &[u8] = b"orrery/d32/ramp-posture/v1\0";

/// The at-rest schema discriminant carried as the first byte of a
/// `ramp/{control}` value (D32 clause (i), [D38]'s idiom).
///
/// **Why 3 and not 1.** The pre-amendment value was untagged
/// `postcard(RampPosture)`, whose very first byte is a postcard varint holding
/// `RampMode`'s discriminant — a three-variant enum, so `0`, `1` and `2` are
/// exactly the byte values an old reader accepts and continues past. A tag in
/// that range is a tag that gets half-read instead of rejected, which is the
/// failure this constant exists to prevent. Schema numbers 0, 1 and 2 are
/// therefore unallocated by construction and the first tagged schema is 3. The
/// tag must also stay below `0x80`, so it is a single-byte postcard varint and
/// the remainder starts at a fixed offset.
///
/// [D38]: ../../../../docs/adr/0038-at-rest-schema-versioning.md
pub const RAMP_POSTURE_SCHEMA: u8 = 3;

/// A `ramp/{control}` value: clause (c)'s posture plus clause (i)'s
/// authenticator and expiry.
///
/// The posture is nested rather than flattened so that the type the pollers,
/// the meter and the auto-suspend monitor already speak
/// ([`RampPosture`]) is unchanged, and so the authenticator cannot be confused
/// for part of it at any call site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedRampPosture {
    /// Clause (c)'s posture, byte-identical in meaning to the untagged value.
    pub posture: RampPosture,
    /// Mandatory on a write that leaves the control below its clause (c)
    /// default; refused without one, and reverted to the startup default once
    /// passed. `None` on a promotion, which needs no expiry.
    pub expires_at_ms: Option<u64>,
    /// Which operator key signed this row. `None` on an `AutoSuspend` row,
    /// which is unsigned by design: a tripping gateway holds no operator key.
    pub signer: Option<NodeId>,
    /// Ed25519 by `signer` over [`posture_preimage`].
    pub signature: Option<Signature>,
}

/// Why a poller refused a durable posture row.
///
/// Every variant is a refusal to *apply a claimed mode*, never an error: a
/// refused row leaves the control at clause (i)'s fallback and is logged, so
/// the row shows up as an incident rather than as a quiet mode change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostureRefusal {
    /// The value did not carry [`RAMP_POSTURE_SCHEMA`], or did not decode.
    Malformed,
    /// An `Operator` row with no signature at all — the raw-cluster-file write.
    Unsigned,
    /// Signed by a key outside this process's `--operator-key` set.
    UnknownSigner,
    /// The signature does not verify: the row was altered after signing, or a
    /// signature was moved here from another control's row.
    BadSignature,
    /// An `AutoSuspend` row selecting anything but `shadow` — clause (f).
    AutoSuspendMayOnlySelectShadow,
    /// `ramp/quarantine_validation = off`: closed open question 3, no such arm.
    NoSuchArm,
    /// A write below the control's clause (c) default with no `expires_at_ms`.
    DeHardeningNeedsExpiry,
    /// The row carried an expiry and it has passed.
    Expired,
}

impl std::fmt::Display for PostureRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Malformed => "value is not a D32 clause (i) posture row",
            Self::Unsigned => "operator row carries no signature",
            Self::UnknownSigner => "signed by a key outside --operator-key",
            Self::BadSignature => "signature does not verify over this control's preimage",
            Self::AutoSuspendMayOnlySelectShadow => "auto-suspend may only select shadow",
            Self::NoSuchArm => "quarantine_validation has no off arm (D32 open question 3)",
            Self::DeHardeningNeedsExpiry => "a de-hardening write must carry expires_at_ms",
            Self::Expired => "the row's expires_at_ms has passed",
        })
    }
}

/// What a poller does with a `ramp/{control}` value it just read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostureVerdict {
    /// No row, or a row whose expiry passed: the CLI startup default applies.
    ///
    /// An expired row is deliberately the *same* verdict as an absent one.
    /// Clause (i) says a de-hardening write reverts to the startup default when
    /// it lapses, and making that indistinguishable from removal is what stops
    /// "the incident ended and nobody remembered the row" from being a state
    /// the fleet can be in.
    StartupDefault,
    /// The row verified and its mode applies.
    Admitted(RampPosture),
    /// The row was refused, and is reported as absent.
    ///
    /// **This is a correction to the accepted spike's text, flagged not
    /// silent.** The spike said a refused row makes the control fall to
    /// `shadow`. Falling back to the *startup default* is what
    /// [`super::ramp::admitted`] already does for the row-class refusal, and
    /// it is the better rule for two reasons the spike did not price. It
    /// denies a forger a lever: under "fall to shadow", anyone who can write
    /// FoundationDB can move all four `off`-default controls into `shadow` and
    /// make the fleet pay clause (d)'s write tax indefinitely, which is the
    /// denial-of-service shape clause (f) refuses elsewhere by name. And it
    /// keeps one fallback in the system instead of two: a refused row lands on
    /// a value an operator chose at launch, whichever of the two checks
    /// refused it.
    Refused(PostureRefusal),
}

impl PostureVerdict {
    /// The mode this verdict selects, given the poller's startup default.
    ///
    /// This is the one function a poller needs, and the reason clause (i)'s
    /// fallback cannot be gotten wrong at a call site: a refusal never returns
    /// the mode the row claimed, because the claimed mode is not reachable from
    /// here.
    #[must_use]
    pub fn mode(&self, startup_default: RampMode) -> RampMode {
        match self {
            Self::StartupDefault | Self::Refused(_) => startup_default,
            Self::Admitted(posture) => posture.mode,
        }
    }
}

/// D32 clause (c)'s startup-default table, as a function of the control name.
///
/// This is the record's own table (`0032-enforcement-ramp.md`, clause (c)) and
/// not the process's CLI arguments, on purpose: "de-hardening" has to mean
/// "below what this software ships as" for the expiry rule to be a property of
/// the fleet rather than of one process's launch line.
#[must_use]
pub fn d32_default(control: &str) -> RampMode {
    if control == super::QUARANTINE_VALIDATION_CONTROL {
        // C2 is the only control D32 ships live.
        RampMode::Live
    } else {
        RampMode::Off
    }
}

/// Does this write leave the fleet acting *below* D32's own default?
///
/// For C1, C4 and C5 the default is `off`, so nothing can go below it and every
/// write is a promotion — the "lever that only weakens" problem #875 warns
/// about does not arise. For C2 alone the lever points downward from shipped
/// behaviour. One control, one rule.
#[must_use]
pub fn is_de_hardening(control: &str, mode: RampMode) -> bool {
    mode.rank() < d32_default(control).rank()
}

/// The signed preimage for one control's posture row.
///
/// See the module docs for the layout and for why the domain constant and the
/// control name are where they are.
#[must_use]
pub fn posture_preimage(control: &str, row: &SignedRampPosture) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(RAMP_POSTURE_V1_DOMAIN);
    hasher.update(
        &u32::try_from(control.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    hasher.update(control.as_bytes());
    hasher.update(&[match row.posture.mode {
        RampMode::Off => 0,
        RampMode::Shadow => 1,
        RampMode::Live => 2,
    }]);
    hasher.update(&[match row.posture.source {
        PostureSource::Default => 0,
        PostureSource::Operator => 1,
        PostureSource::AutoSuspend => 2,
    }]);
    hasher.update(&row.posture.set_at_ms.to_le_bytes());
    hasher.update(
        &u32::try_from(row.posture.reason.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    hasher.update(row.posture.reason.as_bytes());
    match row.posture.incident_id {
        None => hasher.update(&[0u8]),
        Some(id) => {
            hasher.update(&[1u8]);
            hasher.update(&id)
        }
    };
    match row.expires_at_ms {
        None => hasher.update(&[0u8]),
        Some(at) => {
            hasher.update(&[1u8]);
            hasher.update(&at.to_le_bytes())
        }
    };
    *hasher.finalize().as_bytes()
}

/// Sign one operator posture row for `control`.
///
/// The caller supplies the posture and the optional expiry; this fills in the
/// signer and the signature. Signing is deliberately a pure function of the row
/// and the control name — it consults no clock, no cluster and no policy, so
/// the operator tool and any future signer produce byte-identical rows.
#[must_use]
pub fn sign_posture(
    control: &str,
    posture: RampPosture,
    expires_at_ms: Option<u64>,
    key: &iroh::SecretKey,
) -> SignedRampPosture {
    let mut row = SignedRampPosture {
        posture,
        expires_at_ms,
        signer: Some(key.public()),
        signature: None,
    };
    row.signature = Some(key.sign(&posture_preimage(control, &row)));
    row
}

/// Encode a row as the tagged `ramp/{control}` value.
///
/// # Errors
///
/// Propagates a postcard encoding failure, which for this shape means an
/// allocation failure rather than a representable one.
pub fn encode(row: &SignedRampPosture) -> Result<Vec<u8>, postcard::Error> {
    let mut value = Vec::with_capacity(128);
    value.push(RAMP_POSTURE_SCHEMA);
    value.extend_from_slice(&postcard::to_stdvec(row)?);
    Ok(value)
}

/// Decode a tagged `ramp/{control}` value.
///
/// # Errors
///
/// [`PostureRefusal::Malformed`] when the tag is absent or unknown, or when the
/// remainder is not a `SignedRampPosture`. An untagged pre-amendment value
/// lands here too, and that is the point: this reader refuses the old shape as
/// firmly as the old reader refuses the new one, so a rolling upgrade cannot
/// have half the fleet honouring a row the other half rejects.
pub fn decode(value: &[u8]) -> Result<SignedRampPosture, PostureRefusal> {
    match value.split_first() {
        Some((&RAMP_POSTURE_SCHEMA, rest)) => {
            postcard::from_bytes(rest).map_err(|_| PostureRefusal::Malformed)
        }
        _ => Err(PostureRefusal::Malformed),
    }
}

/// Clause (i)'s admission predicate: run by every poller, on every poll, before
/// a row's mode may take effect.
///
/// `operator_keys` is the process's `--operator-key` set. `now_ms` is wall
/// clock, injected rather than read so the expiry rule is testable.
///
/// The order of the checks is not arbitrary. The two that hold regardless of
/// who signed — the closed `off` arm and clause (f)'s auto-suspend asymmetry —
/// run *first*, so a correctly signed row still cannot select an arm that does
/// not exist, and so a forged auto-suspend row is refused without a signature
/// check having to succeed for the refusal to be reached.
///
/// # Errors
///
/// A [`PostureRefusal`] naming which rule the row broke. The caller logs it and
/// falls back per [`PostureVerdict::mode`]; it never sees the claimed mode.
pub fn admit(
    control: &str,
    row: &SignedRampPosture,
    operator_keys: &[NodeId],
    now_ms: u64,
) -> Result<(), PostureRefusal> {
    // D32 open question 3, closed in the negative by clause (i). C2's only
    // demotion is `live -> shadow`; the `off` arm does not exist, so no key
    // holder can select it.
    if control == super::QUARANTINE_VALIDATION_CONTROL && row.posture.mode == RampMode::Off {
        return Err(PostureRefusal::NoSuchArm);
    }

    // Clause (f)'s asymmetry is #876's, and this ANDs onto it rather than
    // restating it. `RampPosture::admissible` is the row-local half — an
    // automation row is admissible only at `shadow` — and
    // `RampPosture::admissible_from` is the transition half, applied by the
    // poller because it is the only thing that knows the acting mode.
    //
    // **Not a rank comparison.** The spike stated this arm as
    // `rank(row.mode) >= rank(current) => refuse`, and that is a defect in the
    // spike: `off` ranks *below* `live`, so a rank test alone admits
    // `AutoSuspend -> off` from a live control — precisely the "induce spikes,
    // blind the cluster" lever clause (f) forbids by name. #876 found it and
    // ships the conjunction; nothing here may loosen it back.
    if !row.posture.admissible() {
        return Err(PostureRefusal::AutoSuspendMayOnlySelectShadow);
    }
    if row.posture.source == PostureSource::AutoSuspend {
        // The two arms are disjoint, which is why they compose without a
        // precedence question: an automation row is unsigned *by design*,
        // because a tripping gateway holds no operator key and must not. So
        // there is nothing for the signature check below to say about it, and a
        // forged demotion is still only a demotion — which clause (f) permits
        // without asking, and which `admissible_from` will still refuse at the
        // poller if it is not a strict lowering.
        return Ok(());
    }

    // Every operator row is authenticated at read time, by the consumer.
    let (Some(signer), Some(signature)) = (row.signer, row.signature) else {
        return Err(PostureRefusal::Unsigned);
    };
    if !operator_keys.contains(&signer) {
        return Err(PostureRefusal::UnknownSigner);
    }
    signer
        .verify(&posture_preimage(control, row), &signature)
        .map_err(|_| PostureRefusal::BadSignature)?;

    // A de-hardening write must say when it stops. An incident demotion that
    // outlives its incident is how a ramp silently un-ships its own hardening,
    // and nothing alerts, because a posture row is supposed to sit there.
    if is_de_hardening(control, row.posture.mode) && row.expires_at_ms.is_none() {
        return Err(PostureRefusal::DeHardeningNeedsExpiry);
    }
    if row.expires_at_ms.is_some_and(|at| at <= now_ms) {
        return Err(PostureRefusal::Expired);
    }
    Ok(())
}

/// Decode and admit one raw `ramp/{control}` value, producing the verdict a
/// poller acts on.
///
/// An expired row becomes [`PostureVerdict::StartupDefault`] rather than a
/// refusal, because clause (i) makes lapsing identical to removal; every other
/// refusal stays a refusal.
#[must_use]
pub fn verdict(
    control: &str,
    value: Option<&[u8]>,
    operator_keys: &[NodeId],
    now_ms: u64,
) -> PostureVerdict {
    let Some(value) = value else {
        return PostureVerdict::StartupDefault;
    };
    let row = match decode(value) {
        Ok(row) => row,
        Err(refusal) => return PostureVerdict::Refused(refusal),
    };
    match admit(control, &row, operator_keys, now_ms) {
        Ok(()) => PostureVerdict::Admitted(row.posture),
        Err(PostureRefusal::Expired) => PostureVerdict::StartupDefault,
        Err(refusal) => PostureVerdict::Refused(refusal),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{AUTHORITY_CORRECTION_CONTROL, STRIKES_CONTROL};

    fn key(byte: u8) -> iroh::SecretKey {
        iroh::SecretKey::from_bytes(&[byte; 32])
    }

    fn posture(mode: RampMode) -> RampPosture {
        RampPosture {
            mode,
            source: PostureSource::Operator,
            set_at_ms: 1_700_000_000_000,
            reason: "incident 4471".to_string(),
            incident_id: None,
        }
    }

    #[test]
    fn a_correctly_signed_operator_row_is_admitted() {
        let operator = key(1);
        let row = sign_posture(STRIKES_CONTROL, posture(RampMode::Live), None, &operator);
        assert_eq!(
            admit(STRIKES_CONTROL, &row, &[operator.public()], 0),
            Ok(())
        );
    }

    #[test]
    fn a_raw_cluster_file_write_is_refused() {
        // The M1 row: exactly what a cluster-file holder can produce, because
        // producing a signature is what they cannot do.
        let row = SignedRampPosture {
            posture: posture(RampMode::Off),
            expires_at_ms: None,
            signer: None,
            signature: None,
        };
        assert_eq!(
            admit(STRIKES_CONTROL, &row, &[key(1).public()], 0),
            Err(PostureRefusal::Unsigned),
            "FoundationDB access is not authority over fleet enforcement"
        );
    }

    #[test]
    fn a_row_signed_by_an_unknown_key_is_refused() {
        let attacker = key(9);
        let row = sign_posture(STRIKES_CONTROL, posture(RampMode::Off), None, &attacker);
        assert_eq!(
            admit(STRIKES_CONTROL, &row, &[key(1).public()], 0),
            Err(PostureRefusal::UnknownSigner)
        );
    }

    #[test]
    fn flipping_the_mode_after_signing_is_refused() {
        let operator = key(1);
        let mut row = sign_posture(STRIKES_CONTROL, posture(RampMode::Live), None, &operator);
        row.posture.mode = RampMode::Off;
        assert_eq!(
            admit(STRIKES_CONTROL, &row, &[operator.public()], 0),
            Err(PostureRefusal::BadSignature)
        );
    }

    /// Failing-if-removed check for the preimage's **second** field.
    #[test]
    fn a_signature_does_not_transfer_between_controls() {
        let operator = key(1);
        // A legitimate, correctly signed demotion of one control...
        let row = sign_posture(
            AUTHORITY_CORRECTION_CONTROL,
            posture(RampMode::Off),
            None,
            &operator,
        );
        assert_eq!(
            admit(AUTHORITY_CORRECTION_CONTROL, &row, &[operator.public()], 0),
            Ok(())
        );
        // ...moved, byte for byte, to another control's key.
        assert_eq!(
            admit(STRIKES_CONTROL, &row, &[operator.public()], 0),
            Err(PostureRefusal::BadSignature),
            "the control name is inside the preimage; drop it and this passes"
        );
    }

    /// Failing-if-removed check for the preimage's **first** field.
    ///
    /// The alternative preimage below is [`posture_preimage`]'s body with the
    /// domain constant and *nothing else* removed, so that deleting the
    /// constant from the real function makes the two hashes equal, makes the
    /// replayed signature verify, and fails this test. A test that hashed some
    /// other byte string would pass either way and prove nothing — which is
    /// what this test did until it was checked by mutation.
    #[test]
    fn the_domain_constant_is_not_decoration() {
        let operator = key(1);
        let row = sign_posture(STRIKES_CONTROL, posture(RampMode::Live), None, &operator);
        let with_domain = posture_preimage(STRIKES_CONTROL, &row);

        // Every field of the real preimage, in the real order, minus the
        // domain separator: exactly what a cross-protocol replay would sign.
        let mut hasher = blake3::Hasher::new();
        hasher.update(&u32::try_from(STRIKES_CONTROL.len()).unwrap().to_le_bytes());
        hasher.update(STRIKES_CONTROL.as_bytes());
        hasher.update(&[2u8]); // Live
        hasher.update(&[1u8]); // Operator
        hasher.update(&row.posture.set_at_ms.to_le_bytes());
        hasher.update(
            &u32::try_from(row.posture.reason.len())
                .unwrap()
                .to_le_bytes(),
        );
        hasher.update(row.posture.reason.as_bytes());
        hasher.update(&[0u8]); // no incident_id
        hasher.update(&[0u8]); // no expires_at_ms
        let without_domain = *hasher.finalize().as_bytes();
        assert_ne!(
            with_domain[..],
            without_domain[..],
            "the domain constant must be inside the hash, not beside it"
        );

        let replayed = SignedRampPosture {
            signature: Some(operator.sign(&without_domain)),
            ..row
        };
        assert_eq!(
            admit(STRIKES_CONTROL, &replayed, &[operator.public()], 0),
            Err(PostureRefusal::BadSignature),
            "a signature made under any other domain is not a posture signature"
        );
    }

    #[test]
    fn an_autosuspend_row_needs_no_signature_but_may_only_select_shadow() {
        let row = |mode| SignedRampPosture {
            posture: RampPosture {
                mode,
                source: PostureSource::AutoSuspend,
                set_at_ms: 1,
                reason: "verdict rate".to_string(),
                incident_id: Some([7; 16]),
            },
            expires_at_ms: None,
            signer: None,
            signature: None,
        };
        assert_eq!(
            admit(STRIKES_CONTROL, &row(RampMode::Shadow), &[], 0),
            Ok(())
        );
        assert_eq!(
            admit(STRIKES_CONTROL, &row(RampMode::Live), &[], 0),
            Err(PostureRefusal::AutoSuspendMayOnlySelectShadow),
            "automation may not promote, however the row reached FoundationDB"
        );
        assert_eq!(
            admit(STRIKES_CONTROL, &row(RampMode::Off), &[], 0),
            Err(PostureRefusal::AutoSuspendMayOnlySelectShadow),
            "and may not blind the cluster during the incident that tripped it"
        );
    }

    /// The composition with #876, pinned so neither lane can loosen the other.
    ///
    /// `admit` is not the whole automation rule and must not become it: it
    /// clears an `AutoSuspend` row that `admissible` allows, and that row still
    /// faces `admissible_from` at the poller. The `shadow -> shadow` case is
    /// the one that shows the two are independent — `admit` says yes, the
    /// transition rule says no, and the row is refused.
    #[test]
    fn admit_ands_onto_876s_predicates_rather_than_replacing_them() {
        let idempotent = SignedRampPosture {
            posture: RampPosture {
                mode: RampMode::Shadow,
                source: PostureSource::AutoSuspend,
                set_at_ms: 1,
                reason: "verdict rate".to_string(),
                incident_id: Some([7; 16]),
            },
            expires_at_ms: None,
            signer: None,
            signature: None,
        };
        assert_eq!(
            admit(STRIKES_CONTROL, &idempotent, &[], 0),
            Ok(()),
            "authentication has nothing to say about an unsigned automation row"
        );
        assert!(
            idempotent.posture.admissible(),
            "and neither has the row-local half"
        );
        assert!(
            !idempotent.posture.admissible_from(RampMode::Shadow),
            "the transition half still refuses it: automation may only STRICTLY \
             lower the acting rank, and the poller is where that is decided"
        );

        // And the defect the spike's rank-only phrasing would have reintroduced:
        // `off` ranks below `live`, so a rank comparison alone would admit this.
        let blinding = SignedRampPosture {
            posture: RampPosture {
                mode: RampMode::Off,
                ..idempotent.posture.clone()
            },
            ..idempotent
        };
        assert!(
            blinding.posture.mode.rank() < RampMode::Live.rank(),
            "a rank comparison alone would call this a demotion"
        );
        assert_eq!(
            admit(STRIKES_CONTROL, &blinding, &[], 0),
            Err(PostureRefusal::AutoSuspendMayOnlySelectShadow),
            "and it is refused anyway: blinding the cluster during the incident \
             that tripped the breaker is clause (f)'s named denial-of-service"
        );
        assert!(!blinding.posture.admissible_from(RampMode::Live));
    }

    #[test]
    fn c2s_off_arm_does_not_exist_even_for_a_key_holder() {
        let operator = key(1);
        let row = sign_posture(
            super::super::QUARANTINE_VALIDATION_CONTROL,
            posture(RampMode::Off),
            Some(u64::MAX),
            &operator,
        );
        assert_eq!(
            admit(
                super::super::QUARANTINE_VALIDATION_CONTROL,
                &row,
                &[operator.public()],
                0
            ),
            Err(PostureRefusal::NoSuchArm),
            "D32 open question 3, closed in the negative"
        );
    }

    #[test]
    fn a_de_hardening_write_without_an_expiry_is_refused() {
        let operator = key(1);
        let control = super::super::QUARANTINE_VALIDATION_CONTROL;
        assert!(is_de_hardening(control, RampMode::Shadow));
        assert!(!is_de_hardening(STRIKES_CONTROL, RampMode::Off));

        let permanent = sign_posture(control, posture(RampMode::Shadow), None, &operator);
        assert_eq!(
            admit(control, &permanent, &[operator.public()], 1_000),
            Err(PostureRefusal::DeHardeningNeedsExpiry),
            "a demotion below the shipped default cannot become permanent"
        );

        let temporary = sign_posture(
            control,
            posture(RampMode::Shadow),
            Some(1_000 + 3_600_000),
            &operator,
        );
        assert_eq!(
            admit(control, &temporary, &[operator.public()], 1_000),
            Ok(())
        );
        assert_eq!(
            admit(control, &temporary, &[operator.public()], 1_000 + 3_600_001),
            Err(PostureRefusal::Expired),
            "past its expiry the poller reverts to the startup default"
        );
    }

    #[test]
    fn a_promotion_needs_no_expiry() {
        let operator = key(1);
        let row = sign_posture(STRIKES_CONTROL, posture(RampMode::Live), None, &operator);
        assert_eq!(
            admit(STRIKES_CONTROL, &row, &[operator.public()], u64::MAX - 1),
            Ok(()),
            "clause (f)'s asymmetry: hardening is permanent, weakening expires"
        );
    }

    #[test]
    fn an_expired_row_is_the_same_verdict_as_no_row_at_all() {
        let operator = key(1);
        let control = super::super::QUARANTINE_VALIDATION_CONTROL;
        let row = sign_posture(control, posture(RampMode::Shadow), Some(500), &operator);
        let value = encode(&row).unwrap();
        assert_eq!(
            verdict(control, Some(&value), &[operator.public()], 1_000),
            PostureVerdict::StartupDefault
        );
        assert_eq!(
            verdict(control, None, &[operator.public()], 1_000),
            PostureVerdict::StartupDefault
        );
    }

    #[test]
    fn a_refused_row_never_yields_the_mode_it_claimed() {
        let forged = SignedRampPosture {
            posture: posture(RampMode::Off),
            expires_at_ms: None,
            signer: None,
            signature: None,
        };
        let value = encode(&forged).unwrap();
        let verdict = verdict(STRIKES_CONTROL, Some(&value), &[key(1).public()], 0);
        assert_eq!(
            verdict,
            PostureVerdict::Refused(PostureRefusal::Unsigned),
            "the forged `off` is refused"
        );
        assert_eq!(
            verdict.mode(RampMode::Live),
            RampMode::Live,
            "a refused row is reported as an absent one, so the control falls \
             back to the startup default an operator chose at launch — never \
             to the claimed mode, and never to a mode a forger selected"
        );
        assert_eq!(
            verdict.mode(RampMode::Off),
            RampMode::Off,
            "and a forger cannot move an off-default control into shadow to \
             make the fleet pay clause (d)'s write tax"
        );
    }

    /// The migration hazard, proven in both directions.
    ///
    /// The pre-amendment reader is `postcard::from_bytes::<RampPosture>` over
    /// the raw value — reproduced here rather than referenced, because the
    /// point of the test is that the *old* code path refuses the *new* bytes,
    /// and the old code path no longer exists to call.
    #[test]
    fn the_pre_amendment_reader_refuses_a_tagged_value() {
        let operator = key(1);
        let row = sign_posture(STRIKES_CONTROL, posture(RampMode::Live), None, &operator);
        let tagged = encode(&row).unwrap();

        assert_eq!(
            tagged[0], RAMP_POSTURE_SCHEMA,
            "the discriminant is at the front, where a prefix decoder must meet it"
        );
        assert!(
            postcard::from_bytes::<RampPosture>(&tagged).is_err(),
            "an un-upgraded gateway must REFUSE a signed row, not half-read it \
             and apply the mode without checking the signature"
        );

        // And the converse: without the tag, the old reader accepts the new
        // shape and silently drops the authenticator. This is the measured
        // hazard, asserted so the tag cannot be removed as redundant.
        let untagged = postcard::to_stdvec(&row).unwrap();
        let half_read: RampPosture = postcard::from_bytes(&untagged)
            .expect("postcard is prefix-tolerant; this is the hazard");
        assert_eq!(
            half_read.mode,
            RampMode::Live,
            "the mode lands, the signature does not — hence the tag"
        );

        // The new reader refuses the old shape just as firmly, so a rolling
        // upgrade cannot have the halves disagree about a row.
        let pre_amendment = postcard::to_stdvec(&posture(RampMode::Live)).unwrap();
        assert_eq!(decode(&pre_amendment), Err(PostureRefusal::Malformed));
    }
}
