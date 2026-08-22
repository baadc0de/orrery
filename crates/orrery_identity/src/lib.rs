//! The identity service (D12): accounts, NodeId binding, and session tokens.
//!
//! D12's service table names Identity first — "Accounts, NodeId binding,
//! session tokens, strike/reputation ledger, bans" — and until this crate
//! existed the tree held the *verification* half of that with no issuer.
//! [`orrery_protocol::identity`] defines `SessionTokenClaimsV1`,
//! `SessionTokenV1::sign`, `SessionTokenVerifier` and its rejection taxonomy;
//! the coordinator and the gateway both verify against a configured issuer-key
//! set. Nothing minted a token outside test code. This crate is the missing
//! half, and it deliberately re-uses the existing one rather than restating it:
//! every token it produces is signed by `SessionTokenV1::sign` and is meant to
//! be checked by `SessionTokenVerifier::verify`.
//!
//! # What is here
//!
//! - [`store`] — the [`AccountStore`] seam: account records, the reverse
//!   binding index, and the append-only binding history, i.e. D31's `da`, `db`
//!   and `dh` rows.
//! - [`mem`] — [`MemAccountStore`], an in-process store for tests and
//!   harnesses. It also implements
//!   [`orrery_persistd::gateway::BindingAuthority`] directly, so a harness that
//!   holds one can answer `owner(n)` without a durable read.
//! - [`fdb`] (feature `fdb`) — [`fdb::FdbAccountStore`], the durable store over
//!   #209's key builders. Every mutation is **one** FoundationDB transaction
//!   that writes `da`, `db` and `dh` together, which is D31 clause (b)'s
//!   requirement and not an optimization: a reverse index maintained in a
//!   second transaction has a window in which `db` names an account `da` no
//!   longer binds, and under clause (f) a *wrong* answer admits where a miss
//!   would have excluded.
//! - [`issuer`] — [`IssuerKeyring`], the signing-key set. More than one key is
//!   held at a time and one is active, which is what makes
//!   `docs/09-services-and-ops.md` §8's "rotate = publish new well-known
//!   NodeId, dual-accept for one client-release cycle" a thing that can be
//!   performed rather than described.
//! - [`service`] — [`IdentityService`], which mints and refreshes.
//!
//! # What is deliberately not here
//!
//! **Standing is read, never written here.** D33 (proposed) puts the `ya`
//! strike ledger behind a separate writer (the adjudication executor).
//! [`standing`] supplies its read-time scorer and configurable boundaries;
//! this crate still takes the result through [`StandingSource`] and stamps it
//! into claims. The default source, [`UnavailableStanding`], **refuses**,
//! which is D33 clause (f)'s posture: "a missing or unreadable ledger is never
//! interpreted as `Good`".
//!
//! **Credentials, payment and account creation UX.** D10 item 5 says an account
//! costs something; pricing it and proving it was paid for is a product
//! decision. [`AccountStore::create_account`] creates one by fiat.
//!
//! **`SessionTokenClaimsV1` is unchanged.** Probation, account-age buckets and
//! any widened standing are sibling work; this crate issues the V1 claims that
//! already exist, including their two-armed
//! [`orrery_protocol::SessionStanding`].
//!
//! # The TTL cap is enforced at both ends
//!
//! [`orrery_protocol::MAX_SESSION_TOKEN_TTL_MS`] is one hour, and the
//! verifier rejects a longer lifetime with `OverTtl`. [`IdentityService`]
//! refuses to *issue* one, so an over-long token is a caller error at the
//! issuer rather than a token that exists and cannot be used. A cap enforced
//! only at the far end is a cap that has already been violated by the time
//! anyone notices.

#![warn(missing_docs)]

pub mod issuer;
pub mod mem;
pub mod service;
pub mod standing;
pub mod store;

#[cfg(feature = "fdb")]
pub mod fdb;

pub use issuer::{IssuerKeyring, IssuerSigningKey, RotationError};
pub use mem::MemAccountStore;
pub use service::{
    IdentityService, IssuedSession, StandingSource, StaticStanding, SystemClock,
    UnavailableStanding, DEFAULT_SESSION_TOKEN_TTL_MS,
};
pub use standing::{
    score_rows, ComputedStanding, StandingLevel, StandingScores, StandingThresholds,
    StaticStrikeRows, DEFAULT_STANDING_THRESHOLDS,
};
pub use store::{AccountStore, BindOutcome, IdentityError};
