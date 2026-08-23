//! Pure component-schema migration functions (D38 clause (e)).
//!
//! A migrator is rules code: it receives only the component payload and the
//! version that payload declares, and returns the next adjacent version's
//! payload. Keeping the function pointer in the headless core gives game
//! migrators the same static determinism gates as `Ruleset` implementations;
//! registration and persistence remain composition concerns in `persistd`.

use bytes::Bytes;
use orrery_protocol::atrest::SchemaVersion;

/// One pure `v -> v + 1` component migration step.
///
/// The function must not perform I/O, read a clock or global state, or depend
/// on unordered iteration. `from_version` is supplied explicitly so one
/// implementation may serve several adjacent registrations without ambient
/// configuration.
pub type ComponentMigrator =
    fn(payload: Bytes, from_version: SchemaVersion) -> Result<Bytes, &'static str>;
