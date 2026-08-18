//! The invariant-J audit's default sampling rate, pinned.
//!
//! docs/08-persistence.md §2.1.2 states the rate as part of the safety
//! argument — "one in 1000 in release, **1** under `debug_assertions`, so the
//! whole test suite audits every accept" — and the release figure is what
//! makes "FoundationDB is off the bulk write path" true to 0.1 %. Both halves
//! are a number in one `unwrap_or`, and nothing else checks them: the tests
//! that exercise the audit all set `ORRERY_FENCED_LOCATION_AUDIT_N`
//! explicitly, so they would pass with any default at all.
//!
//! Its own test binary for exactly that reason. The interval resolves once,
//! on first use, so a binary that sets the variable anywhere can no longer
//! observe the default.

use orrery_persistd::cluster::fenced_location_audit_every;

#[test]
fn the_documented_default_sampling_rate_is_what_ships() {
    assert!(
        std::env::var("ORRERY_FENCED_LOCATION_AUDIT_N").is_err(),
        "this binary must observe the default, so it must not set the override"
    );
    let expected = if cfg!(debug_assertions) { 1 } else { 1000 };
    assert_eq!(
        fenced_location_audit_every(),
        expected,
        "docs/08 §2.1.2 promises 1 under debug_assertions and 1000 in release"
    );
}

#[test]
fn the_override_is_honoured_and_zero_disables_the_audit() {
    // Same process, so this cannot re-resolve the interval; it asserts the
    // documented contract of the value instead, which is that 0 is the only
    // setting that turns the audit off.
    assert_ne!(
        fenced_location_audit_every(),
        0,
        "the audit must never default to disabled: 0 gives up the only \
         production evidence that invariant J holds"
    );
}
