//! Protocol versioning (D15).

/// The wire protocol version. Services accept **this version only**.
///
/// # Version 3, and why the rolling-upgrade window is gone
///
/// [D29](https://github.com/baadc0de/orrery/blob/main/docs/adr/0029-low-population-path.md)
/// clause 5 appends a third arm to
/// [`IntentOutcome`](crate::IntentOutcome). postcard keys enum variants by
/// declaration order, so appending an arm is safe to encode-old/decode-new and
/// unsafe in the other direction: a version-1 client handed `Provisional`
/// fails to decode it. That is the first real protocol break since this
/// constant was introduced, and the operator's decision on accepting D29 was
/// to take it with **the window closed** — the cluster supports version 2 and
/// nothing else.
///
/// The record states the reasoning and this comment repeats it because the
/// constant is where a reader looks: the system is pre-release and has no
/// external clients, so dual-version support was complexity that had not been
/// earned, and keeping the window would have meant carrying a second admission
/// branch — "a version-1 client is refused the provisional path" — through
/// every site D29 touches, to serve a client that does not exist.
///
/// **The closure is once, for all traffic, not per message family.** A
/// version-1 peer cannot decode an intent reply, so letting it negotiate at
/// all would only move the failure to the first low-population commit in the
/// cell it happens to be standing in.
///
/// The version is enforced by
/// [`GatewayMsg::protocol_accepted`](crate::GatewayMsg::protocol_accepted)
/// against every client, because
/// [`GatewayMsg::VersionedHello`](crate::GatewayMsg::VersionedHello) is now the
/// only bootstrap a gateway admits. The unversioned
/// [`GatewayMsg::Hello`](crate::GatewayMsg::Hello) is retired as a wire
/// bootstrap and is refused with
/// [`GatewayReply::HelloRefused`](crate::GatewayReply::HelloRefused): a
/// bootstrap that names no version cannot be shown to be inside a window of
/// one, and admitting it unchecked was the hole that made enforcement opt-in.
///
/// Version 3 adds `candidate_accounts` to
/// [`WitnessEpochClaimsV1`](crate::WitnessEpochClaimsV1). This is a positional
/// postcard change to a signed wire body, so a version-2 peer must not share a
/// gateway session with a version-3 peer.
///
/// Version 4 adds `on_probation` to
/// [`SessionTokenClaimsV1`](crate::SessionTokenClaimsV1), for the same reason
/// and by the same rule. The token rides
/// [`GatewayMsg::VersionedHello`](crate::GatewayMsg::VersionedHello), so a
/// version-3 client presents a version-3 client's token: seven fields where a
/// version-4 service reads eight. postcard appends, and a decoder built for the
/// longer body fails outright on the shorter one rather than ignoring what is
/// missing — there is nothing to ignore, because the body carries no field
/// names. Bumping here is what keeps that failure at the handshake, where it is
/// one refusal with a version in it, instead of at the first token decode.
///
/// Version 5 appends
/// [`GatewayReply::AuthorityCorrection`](crate::GatewayReply::AuthorityCorrection).
/// postcard keys enum variants by position and rejects trailing bytes, so a
/// version-4 peer cannot safely share a session on which that reply may appear.
/// Exact-equality admission makes the incompatibility a handshake refusal
/// instead of a decode failure at the first guilty verdict.
///
/// The version-1 claims body remains *decodable* — see
/// [`SESSION_TOKEN_V1_VERSION`](crate::SESSION_TOKEN_V1_VERSION) — but that
/// window is for a fleet mid-rollout, where identity and the gateways are
/// separate services with no handshake between them. It is not a second
/// client-facing window: this one is exact equality and D29 clause 5 closed it.
///
/// Version 7 appends [`RecordKind::Restore`](crate::RecordKind::Restore).
/// Although that kind is server-owned and refused on client ingress, it is a
/// variant of the `RecordKind` carried by `DiffUplink`; exact-version
/// admission therefore keeps old peers from sharing a positional enum with a
/// build whose type has grown.
///
/// Version 8 adds the hit-registration datagram family
/// ([`HitMsg`](crate::HitMsg) under [`TAG_HIT`](crate::channels::TAG_HIT),
/// docs/05 §7). No existing byte moves — the family is a new sub-tag — but a
/// version-7 peer *silently drops* it: `decode_hit` returns `None` for a tag
/// it does not know, the shooter's claim is never answered, and it is resent
/// until the shooter gives up, with no refusal anywhere. Exact-version
/// admission turns that into one handshake refusal with a version in it,
/// which is the same reason every bump above was taken.
pub const PROTOCOL_VERSION: u16 = 8;
