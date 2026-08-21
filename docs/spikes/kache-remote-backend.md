# Reference: kache 0.14.2's remote backends, and why the remote is native S3

**Status: reference, non-normative.** The decision it records was taken in
[#170](https://github.com/baadc0de/orrery/issues/170) ("Cloud choice: AWS,
settled", 2026-08-21); this document records the source-level facts behind it so
nobody re-proposes the rejected alternative without knowing it was considered.
It supports [#172](https://github.com/baadc0de/orrery/issues/172) and unblocks
[#174](https://github.com/baadc0de/orrery/issues/174) (bucket + lifecycle
provisioning).

**Date:** 2026-08-21. **Sources:** kache 0.14.2 as installed at
`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/kache-0.14.2/` — every
citation below is `config.rs` or `remote_backend.rs` under that directory unless
marked otherwise, and every line number was read directly this session.

---

## 1. What backends exist: exactly two

The backend enum is closed and compiled in, not pluggable
(`config.rs:343-347`):

```rust
pub enum RemoteBackendConfig {
    S3(S3RemoteConfig),
    Filesystem(FilesystemRemoteConfig),
}
```

The S3 half (`config.rs:349-356`) carries four fields and nothing else:

| Field | Type | Line |
|---|---|---|
| `bucket` | `String` | `config.rs:351` |
| `endpoint` | `Option<String>` | `config.rs:352` |
| `region` | `String` | `config.rs:353` |
| `profile` | `Option<String>` | `config.rs:355` |

The filesystem half (`config.rs:358-363`) carries `root: PathBuf` and
`atomic_write_dir: PathBuf`. Both sit behind `RemoteConfig { prefix, backend }`
(`config.rs:336-341`), where `prefix` defaults to `"artifacts"`.

This is not a configuration limit but a compilation one. kache's `Cargo.toml`
depends on exactly two OpenDAL service crates, unconditionally and with no
`[features]` section that could gate them: `opendal-service-fs`
(`Cargo.toml:208-210`) and `opendal-service-s3` (`Cargo.toml:212-214`), plus
`reqsign-aws-v4` for request signing (`Cargo.toml:227-229`). Upstream's
configuration reference says the same thing in words — other OpenDAL services
"are not included in the kache binary", as quoted in #172.

The type-string parser agrees. `[cache.remote] type` accepts `"filesystem"`
or `"fs"`, or `"s3"` (`config.rs:1545-1561`); **any other value is a hard
error** naming the two supported types (`config.rs:1562-1566`). With `type`
absent, kache infers from which field set is populated, and refuses a table
that mixes both (`config.rs:1567-1573`). There is no `gcs` string anywhere in
the dispatch.

## 2. How a remote is configured

### Config file discovery

Priority, from `resolve_config_path` (`config.rs:2088-2124`):

1. `$KACHE_CONFIG` — an explicit path, shell-expanded (`config.rs:2092`);
2. the nearest `.kache.toml` walking **up** from the working directory through
   all ancestors (`PROJECT_CONFIG_NAME`, `config.rs:2014`;
   `nearest_project_config_path`, `config.rs:2126-2135`);
3. the user config: `$XDG_CONFIG_HOME/kache/config.toml`, falling back to
   `~/.config/kache/config.toml` (`config_file_path`,
   `config.rs:2137-2147`).

One file wins outright — there is no merging of the project file with the user
file. The daemon fingerprints whichever file it loaded and self-restarts when
the bytes change, so edits take effect on the next build without a manual
daemon stop (`config.rs:2097-2105`). Note the consequence for our worktrees:
a `.kache.toml` at a worktree root shadows the machine-global
`~/.config/kache/config.toml` for every build beneath it, because cargo
invocations run from inside the worktree.

### TOML tables

The file's top-level tables are `cache`, `cc`, `paths` and `workspace`
(`FileConfig`, `config.rs:527-537`). The remote lives at `[cache.remote]`
(`RemoteFileConfig`, `config.rs:626-644`) with keys `type`, `bucket`,
`endpoint`, `region`, `prefix`, `profile`, `path`, `atomic_write_dir` — one
flat table serving both backends, discriminated by `type`. `[cache.planner]`
(`config.rs:646-654`) is the separate planner-service endpoint, not a remote.

Unknown keys are **silently dropped**, deliberately: serde failing the whole
parse on one typo was judged worse than ignoring it, on the reasoning recorded
at `config.rs:616-625`. A typo'd key does not merely do nothing — it can
silently change behaviour (their measured example: a typo'd `bukcet` drops the
remote *and* leaves no error to show). Check spelling against the structs
above, not against intuition.

### Environment variables, and what beats what

Env wins over file for every gated variable, unless the pinned config sets
`[cache] ignore_env = true` — file-only by design, so an env var cannot
re-enable env (`env_or_ignored`, `config.rs:929-935`; the suppression warns
once, loudly, `config.rs:944-960`). `KACHE_DISABLED` is the exception and is
never gated (`config.rs:983-987`).

For the S3 remote specifically:

| Variable | Overrides | Notes |
|---|---|---|
| `KACHE_S3_BUCKET` | `[cache.remote] bucket` | set-but-empty **disables** the remote rather than falling back to the file's bucket (`config.rs:1628-1650`) |
| `KACHE_S3_ENDPOINT` | `endpoint` | `config.rs:1670-1672` |
| `KACHE_S3_REGION` | `region` | default `"us-east-1"` if neither source sets it (`config.rs:1674-1677`) |
| `KACHE_S3_PREFIX` | `prefix` | default `"artifacts"` (`config.rs:1679-1683`) |
| `KACHE_S3_PROFILE` | `profile` | `config.rs:1685-1689` |

Credentials are not fields of the config at all. A static pair
`KACHE_S3_ACCESS_KEY` + `KACHE_S3_SECRET_KEY` is honoured only when **both**
are set — a partial pair is warned about and ignored
(`remote_backend.rs:716-730`) — and is pushed to the front of the credential
chain when present. Without them, kache uses reqsign's default AWS chain in
AWS-SDK precedence order: environment credentials, then shared-file providers
under the selected profile, then workload identity —
`AssumeRoleWithWebIdentity` — then ECS and IMDSv2 instance roles
(`remote_backend.rs:489-506`). `profile` selects a shared-file profile by
overriding only `AWS_PROFILE` in the lookup environment
(`remote_backend.rs:450-476`). One further endpoint knob exists below the
config layer: `AWS_ENDPOINT_URL_S3` is consulted when `endpoint` is unset
(`remote_backend.rs:702-707`).

Two properties matter for #173/#174. First, **an OIDC-assumed role needs no
kache-specific configuration at all** — it rides the standard chain, which is
why the epic's credential shape composes cleanly with the wrapper. Second,
kache pins the checksum to Content-MD5 for transport integrity
(`remote_backend.rs:695-701`), a header its comment notes is supported by AWS
S3 and common S3-compatible endpoints — support that is *per-endpoint* and
therefore part of what an emulated endpoint must prove.

## 3. The local size cap: AGENTS.md is right

[AGENTS.md](../../AGENTS.md) §Build cache claims (lines 527-528) that
`cache.local_max_size` is the size cap and `max_size` is accepted but silently
ignored, leaving the 50 GiB default. **Verified correct against the source:**

- `CacheFileConfig` (`config.rs:552-614`) defines `local_max_size`
  (`config.rs:555`) and has no `max_size` field;
- unknown keys are dropped silently (`config.rs:616-625`, quoted above), so a
  `max_size = "…"` line under `[cache]` vanishes without error;
- the runtime value resolves in exactly this order: `KACHE_MAX_SIZE` env, then
  `[cache] local_max_size`, then the literal 50 GiB
  (`config.rs:1008-1019`, `unwrap_or(50 * 1024 * 1024 * 1024)`).

One precision worth adding, not a correction: the *name* is only dead in TOML.
`KACHE_MAX_SIZE` as an **environment variable** is honoured — it is first in
the resolution order above and tracked as an active override
(`config.rs:661`, `config.rs:690`). Anyone scripting a size cap per-process
can use it; anyone writing a config file must spell it `local_max_size`.

The companion claim — that neither `CacheFileConfig` nor `RemoteFileConfig`
has any remote size or retention key (AGENTS.md:532-534) — is also
structurally true: the two structs contain no such field, which is the shape
of upstream [kache#774](https://github.com/kunobi-ninja/kache/issues/774). The
nearest thing, `[cache] gc_max_age_hours` (`config.rs:596`), is age retention
for unattended GC sweeps of the **local** store (`config.rs:114-117`), not the
remote. Retention for a remote therefore has to come from the storage layer —
which is an argument for S3 lifecycle rules, not against them.

## 4. The decision: native AWS S3, and why GCS-via-S3-API was rejected

**Decided in #170 (2026-08-21): the kache remote is a native AWS S3 bucket —
`type = "s3"`, no `endpoint` override — provisioned by #174.**

The GCS route — a Google Cloud Storage bucket reached through its
S3-compatible XML API via `endpoint` — was considered and rejected:

1. **It is the emulation path by construction.** kache's backend is natively
   S3 (§1); pointing `endpoint` somewhere else means asking an emulated
   endpoint to satisfy kache's exact request set. That is a real integration
   risk whose only remedy was proof before commitment — a `kache sync
   --dry-run` against a throwaway bucket, per #172's acceptance criteria.
   Choosing AWS removes the risk instead of spending effort mitigating it.
2. **Credentials diverge.** Against GCS, the static-key path above becomes
   HMAC keys for a Google service account — provisioned and rotated separately
   from normal GCP service-account auth — while the OIDC-assumed-role shape
   the epic standardised on (#173) has no GCS analogue through kache's AWS
   credential chain.
3. **Behaviour differs where it hurts.** Multipart upload behaviour differs
   between GCS's emulation and real S3, and the Content-MD5 pinning above is
   exactly the kind of per-endpoint contract an emulator has to match rather
   than be assumed to match.
4. **The cloud choice was made on grounds independent of this issue** —
   billing separation from an unrelated GCP project, blast radius, and
   well-trodden GitHub Actions OIDC federation (all recorded in #170). Given
   AWS, native S3 is strictly the native path.

This paragraph exists so that the rejection is on the record with its
reasons. If a future proposal reaches for `endpoint` against any non-AWS
store — GCS included — it is re-opening a settled question and owes the proof
obligation above, not just a config diff.

## 5. What #174 inherits

A minimal remote config, using only keys verified above:

```toml
[cache]
local_max_size = "15GiB"        # NOT max_size — silently ignored (§3)

[cache.remote]
type   = "s3"
bucket = "orrery-kache"
region = "eu-central-1"
prefix = "artifacts"            # the default anyway; spelled out for greppability
```

No `endpoint` line — its presence is what would select the emulation path. No
credentials in the file: CI authenticates via the workflow's OIDC token
through the standard chain (§2), and developer machines via their normal AWS
profile (`KACHE_S3_PROFILE` or ambient `AWS_PROFILE`). Retention is an S3
lifecycle rule on the bucket, sized against fill rate — with the constraint,
learned here in blood and recorded at AGENTS.md:541-544, that the policy must
key on creation age, since an age-since-last-read policy provably cannot fire
on a continuously-read cache.
