# Always-on P1 swarm host (#474)

This is a deployment runbook, not an instruction to deploy from this change.
The unit runs a sequence of ordinary `p1-swarm --external-peer` processes.
The harness offers the campaign's configured human-seat count during a lobby,
then each attempt still ends after `seconds`; trying to make one process host
indefinitely would contradict that interface. The supervisor waits for the
child, preserving no child process or socket across a restart, then starts the
next attempt after five seconds. Its reports are stored in a new
`attempt-*/raw.json` directory each time.

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
`/var/lib/orrery-p1-swarm/shakedown`. Also create admission's campaign
directory before starting either unit, owned by its sole writer but readable
by the swarm's supplementary group:

```sh
install -d -o orrery -g orrery -m 0750 /var/lib/orrery-p1-swarm/shakedown
install -d -o orrery-admission -g orrery-admission -m 0750 /var/lib/orrery-admission/shakedown
systemctl daemon-reload
systemctl enable --now orrery-p1-swarm
systemctl status orrery-p1-swarm
```

### Reservation-journal co-location is a security assumption

The seat binding deliberately depends on `orrery-admission.service` and
`orrery-p1-swarm.service` running on the same hel1 host and seeing the same
`/var/lib/orrery-admission/shakedown/slots.json`. Nothing in the protocol
guarantees that placement. The swarm unit joins the `orrery-admission` group
and receives read-only access to that campaign directory; make the directory
group-traversable (`0750`, group `orrery-admission`) while keeping admission
its only writer. If either service moves off hel1, replace this with the
deferred signed reservation grant before admitting anyone.

The journal is authoritative over a client's claimed slot. An unreadable,
undecodable, expired, or wrong-generation journal refuses the join. A session,
node, or slot mismatch is also a named refusal; the host never corrects the
client to the journal's seat and never treats journal unavailability as allow.

Confirm `listening.txt` contains the stable host NodeId and UDP port 41641,
and verify UDP reachability from a different network.  The admission service
uses the same `always_on` flag: it mints the ordinary token bound to the
client's transport key, reads that listening record over its existing SSH path,
and returns it instead of launching a second harness. For a multi-human
campaign, admission reads the supervisor-written `attempt.json` in that same
directory and uses its generation and `expires_at` as the reservation lease:
the short allocation lock assigns the earliest free human slot, and all leases
expire together at the attempt boundary. Configure the friends campaign so
`peers + humans = 8`; human seats extend the bot count, rather than consuming
it. The first 90 seconds are the lobby. A full lobby starts immediately; at
the deadline the host sends the same frozen `StartV1` active roster to every
connected human. After that membership is frozen until the next attempt: a
seat that was empty at Start remains empty, and a vacated seat is not reused.

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
issuer-signed, client-key-bound admission gate plus the configured bounded
human-slot count: unauthenticated dials are rejected before becoming a session,
and no more than the configured cohort can consume the simulation.  Keep UDP 41641 open, monitor the
unit/journal, and do not treat this as host authentication.  Rate limiting or
a non-derivable host credential is future work if observed handshake load
requires it; enabling relay fallback is deliberately out of scope.
