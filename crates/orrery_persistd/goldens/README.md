# Migration fixture provenance

Each `*.postcard.hex` file is the exact raw `ComponentBag` postcard byte
stream, rendered as lowercase hexadecimal so the otherwise non-text fixture
can be reviewed in Git.  `fixture_bytes` in `src/migration.rs` is the only
test helper that decodes that representation; it never calls the production
encoder to make an input fixture.

| File | Writer and provenance |
| --- | --- |
| `component-17-v0.postcard.hex` | The W1 `ComponentBag::encode` implementation at `46f116b4e7a7050042f72a9b50139f014ca83bdc` (2026-08-23), the commit immediately before migration machinery `6796f23`. It encodes component 17, schema v0, payload `old`. |
| `component-17-v1.postcard.hex` | The registered v0-to-v1 migrator at `ae4827fdef39d21b2a45ed060f16b549d4b51454` (2026-08-30), encoding component 17, schema v1, payload `old\\x01`. This is the committed expected re-encoding, not a value produced during the test. |
| `component-17-v2.postcard.hex` | The same W1 encoder at `ae4827fdef39d21b2a45ed060f16b549d4b51454`, used to make the deliberately future v2 refusal input for the v1 reader. |
| `component-18-v0.postcard.hex` | The W1 encoder at `ae4827fdef39d21b2a45ed060f16b549d4b51454`, used to make a persisted component for which this composition has no module declaration. |

The older v0 input is the compatibility boundary.  Do not replace it by
encoding a current `ComponentBag` in a test: that would turn this fixture back
into an encode-decode-encode self-check.
