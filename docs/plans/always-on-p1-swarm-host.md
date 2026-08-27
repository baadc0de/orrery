# Always-on P1 swarm host (#474)

This is a deployment runbook, not an instruction to deploy from this change.
The unit runs a sequence of ordinary `p1-swarm --external-peer` processes.
The harness has exactly one external slot and ends after `seconds`; trying to
make one process host indefinitely would contradict that interface.  The
supervisor waits for the child, preserving no child process or socket across a
restart, then starts the next attempt after five seconds.  Its reports are
stored in a new `attempt-*/raw.json` directory each time.

Add this to the live campaign:

```ini
[shakedown]
always_on = yes
```

Install `/opt/orrery/bin/p1-swarm-always-on.py`,
`/etc/systemd/system/orrery-p1-swarm.service`, and a root-owned,
mode-0644 `/etc/orrery/p1-swarm-issuer.pub` containing only the issuer key id
and public key (`<id>:<hex>`).  The existing admission mint output already
names these as `issuer_key_id` and `issuer_public_key`; combine those two
public values.  Do not copy the issuer credential.  Create and give `orrery:orrery` ownership of
`/var/lib/orrery-p1-swarm/shakedown`, then run:

```sh
systemctl daemon-reload
systemctl enable --now orrery-p1-swarm
systemctl status orrery-p1-swarm
```

Confirm `listening.txt` contains the stable host NodeId and UDP port 41641,
and verify UDP reachability from a different network.  The admission service
uses the same `always_on` flag: it mints the ordinary token bound to the
client's transport key, reads that listening record over its existing SSH path,
and returns it instead of launching a second harness.  It still permits one
admission at a time for the configured session duration.

A restart ends the in-progress client session.  The client must rejoin after
the next listening record appears; the partial report is retained but is not a
bankable completed session.  This is intentional and explicit rather than a
claim of seamless handover.

## Exposure and mitigation

The public UDP socket and deterministic host NodeId make this a standing,
discoverable QUIC endpoint.  Its identity authenticates nothing: anyone can
derive it and direct unsolicited handshakes at it.  The immediate threat is
connection/handshake and bandwidth or CPU exhaustion; it is not identity
squatting, because clients already know the public key and no authority trusts
the host identity.  The cheapest adequate mitigation is the existing
issuer-signed, client-key-bound admission gate plus one accepted external slot:
unauthenticated dials are rejected before becoming a session, and only one
admitted peer can consume the simulation.  Keep UDP 41641 open, monitor the
unit/journal, and do not treat this as host authentication.  Rate limiting or
a non-derivable host credential is future work if observed handshake load
requires it; enabling relay fallback is deliberately out of scope.
