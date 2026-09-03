# orrery_ipc_transport

The transport #920 specifies, beside the [`orrery_ipc`] codec, not inside it:
a reliable byte stream with an outer `u32` length prefix and a 1 MiB cap,
behind `Read + Write` so the transport underneath is a one-line swap — and
the #920 sidecar IPC measurement harness that rides on it.

`orrery_ipc` is a codec: no I/O, no outer length prefix, `decode` refuses
trailing bytes, and `MAGIC = b"ORIP"` can occur inside an opaque input
payload. On a byte stream without a prefix, one misaligned read is
unrecoverable — so the prefix lives here, in new code beside the codec.
This crate is Bevy-free (enforced mechanically by `core-gates.sh`'s
`DECLARED_BEVY_FREE_CRATES`) and holds exactly two `unsafe` blocks: reading
`CLOCK_MONOTONIC` (Unix) and `QueryPerformanceCounter` (Windows) — the
system-wide monotonic clock the measurement subtracts across two processes.
There is no safe std equivalent that is cross-process-comparable, and the
codec's own `#![forbid(unsafe_code)]` stands.

## The harness

Two processes, one clock, the phases the decision is taken on:

```
orrery-ipc-bench observer --port 0 --entities 24 --ticks 36000 --report run24.json
orrery-ipc-bench sidecar  --addr 127.0.0.1:PORT --entities 24
```

The observer prints the address it bound (port `0` = an ephemeral OS-chosen
port, which is why the harness needs no port lease); pass it to the sidecar.
The sidecar connects, both sides verify `--entities`/`--hz` agree, and the
run starts on a shared start instant. The observer's report is the artifact
`scripts/ipc-report.py` reads; the sidecar's is supporting evidence.

Both processes read the same system-wide monotonic clock — never the wall
clock, never a per-process epoch — so `t_send_ns` written by one subtracts
cleanly against a reading taken by the other.

### Linux (informational)

```
cargo run --release -p orrery_ipc_transport --bin orrery-ipc-bench -- \
    observer --port 0 --entities 24 --ticks 36000 --report run24.json
cargo run --release -p orrery_ipc_transport --bin orrery-ipc-bench -- \
    sidecar --addr 127.0.0.1:PORT --entities 24
python3 scripts/ipc-report.py run24.json
```

A Linux report renders with `INFORMATIONAL ONLY` attached: the #920 bands are
defined at N = 24 **on Windows**, and no Linux number takes the decision.

### Windows — the deciding measurement

Same invocation on `windows-latest` (or any Windows box), twice, because
#920 lie 1 asks for both:

```powershell
cargo run --release -p orrery_ipc_transport --bin orrery-ipc-bench -- `
    observer --port 0 --entities 24 --ticks 36000 --report run24-tb.json --time-period
cargo run --release -p orrery_ipc_transport --bin orrery-ipc-bench -- `
    sidecar --addr 127.0.0.1:PORT --entities 24 --time-period
python3 scripts/ipc-report.py run24-tb.json
```

`--time-period` raises the timer resolution to 1 ms (`timeBeginPeriod(1)`);
without it, Windows quantizes blocking waits to 15.6 ms. Run with and
without and keep both reports — the gap between them is the timer
granularity, not the transport. The report script prints the verdict —
**SIDECAR STANDS** (p99 ≤ 1 ms, p99.9 ≤ 4 ms, zero dropped
spawn/despawn/input, frame drops ≤ 0.1 %), **SIDECAR OVERTURNED** (p99 ≥
16.7 ms or p50 ≥ 1 ms), or **OWNER'S CALL** — only for a `platform:
"windows"` report.

### Measurement hygiene the harness already enforces

- `TCP_NODELAY` on both ends before any byte moves (lie 2).
- No writes from inside the tick: the writer is a separate thread with a
  bounded latest-wins lane for frames and a reliable FIFO for inputs and
  spawn/despawn batches; every supersession is counted, never hidden
  (lies 3, 4).
- One input per tick from the game thread, one 8-byte null probe on its own
  expendable lane: `hop_null` (pure transport, both one-way legs) and
  `extract_inproc` (same extraction and step consumed in-process, no encode)
  ride the same run, so the transport tax is separable from extraction (lie 5).
- `phase` — the wait for the next engine tick — is recorded and reported
  separately, and excluded from `ipc_added`, which equals
  `hop_in + extract + encode + hop_out + decode_out` (lie 6).
- Headline numbers are unpinned, default power; load average at start and
  end is in the report, so a polluted run identifies itself (lie 7).
- I/O is blocking `std` on dedicated threads; no async runtime is assumed
  (lie 8).
- 600 warmup ticks (10 s) are excluded from every sample (lie 9).

### What the harness does not model

The sidecar is event-driven (it answers the moment input arrives), so no
scheduling wait is hidden inside its columns. The extraction contract is
shape-faithful — iterate N entities, read state, build real
`orrery_ipc::EntityFrame` values, encode and decode the real codec — but it
is not a Bevy `App`; the rules step is O(N) integer work on both sides and
cancels out of the comparison. Churn (a spawn every 600th tick, its despawn
300 later) exercises the reliable path; corrections are not generated.
