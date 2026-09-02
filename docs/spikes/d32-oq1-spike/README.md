# D32 open question 1 spike — posture-write authentication

The working artifact behind
[`../d32-oq1-posture-write-authentication.md`](../d32-oq1-posture-write-authentication.md).
It runs the three candidate mechanisms for authenticating a `ramp/{control}`
write end to end against a real FoundationDB cluster, measures clause (c)'s 2 s
bound against that cluster, and demonstrates the clause (f) direction rule and
#875's C2 de-hardening hazard.

**This is not the writer, and nothing in the workspace can reach it.** D32 open
question 1 must be answered in the record before a writer is built
([#863](https://github.com/baadc0de/orrery/issues/863),
[#875](https://github.com/baadc0de/orrery/issues/875)); this exists so that
answer is chosen against something running.

## Why the manifest is `Cargo.toml.txt`

`check.sh --self-test` discovers every directory declaring `[workspace]` within
four levels of the repository root and dies on any that no lane visits — "a
workspace no lane visits is a workspace nothing checks"
(`scripts/check.sh:635-648, 714-718`). That rule is right, and a propose-only
spike should not buy an exemption from it or hide one directory deeper. So the
manifest ships inert, and running the spike is an explicit act:

```sh
mkdir -p /tmp/d32-oq1-spike/src
cp Cargo.toml.txt /tmp/d32-oq1-spike/Cargo.toml
cp main.rs        /tmp/d32-oq1-spike/src/main.rs
cd /tmp/d32-oq1-spike && FDB_CLUSTER_FILE=/path/to/fdb.cluster cargo run
```

## What it needs

- The FoundationDB **client** 7.3.x. The headers are a *compile* input, not just
  a link input: `foundationdb-gen` does
  `include_bytes!("/usr/include/foundationdb/fdb.options")`. Same package
  `.github/actions/foundationdb/action.yml` installs.
- A running cluster. A single-node in-memory one is enough:
  `fdbserver -p 127.0.0.1:4689 -d <data> -L <logs> -C <cluster-file>`, then
  `fdbcli -C <cluster-file> --exec 'configure new single memory'`.

It touches only `ramp/strikes` and `ramp/quarantine_validation`, clears both on
the way out, and exits non-zero if any check fails.
