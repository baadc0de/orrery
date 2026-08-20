//! One sibling-topology peer, as its own OS process.
//!
//! The difference from `p3-island`'s single-gateway island peer is the whole
//! point of the harness: this peer holds a session to **both**
//! gateways at once and addresses every entity to the gateway whose `--shard`
//! set covers the cell that entity was seeded into. Its interest grant is one
//! grant, minted by one coordinator, presented to both.
//!
//! Two sessions from one process under one [`iroh`] identity is not a
//! multiplexing trick: the gateways are separate processes, each with its own
//! session registry keyed by `NodeId` (`gateway.rs`'s `activate`), so neither
//! can see — or retire — the other's session for this peer.
//!
//! The peer must also **survive** losing one of the two gateways, because the
//! criterion this harness exists to prove is what happens to the *other* one's
//! leases when a sibling is `kill -9`ed. A peer that exited on a torn
//! connection would take its surviving leases down with it and the gate would
//! measure the harness instead of the registrar.

use std::collections::BTreeMap;
use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result};
use orrery_protocol::{
    CellId, ClaimBasis, ClaimId, ClaimKind, GatewayMsg, GatewayReply, GridId, LeaseId, LeaseMsg,
    PersistId, RecordKind, SeqPair, Tick,
};

use crate::wire::Session;

/// D16 lease cadence: renew every 2.5 s against a 10 s TTL.
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(2_500);
/// Bulk uplink cadence, at the top of the 1–4 Hz per-entity band (D11 §2.1).
const UPLINK_INTERVAL: Duration = Duration::from_millis(250);

/// Which of the two sibling gateways a row belongs to.
///
/// The name is the *shard set*, not the process: a peer routes by the cell's
/// owner, and the owner is a property of the `--shard` flags the gate handed
/// each persistd. Carried on every event so the orchestrator can attribute a
/// disposition to the gateway that made it without re-deriving the routing.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The first shard subtree, served by gateway A.
    A,
    /// The second shard subtree, served by gateway B.
    B,
}

impl Side {
    /// The other gateway's side.
    pub fn other(self) -> Self {
        match self {
            Side::A => Side::B,
            Side::B => Side::A,
        }
    }
}

/// One seeded row, and the gateway that owns it.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Row {
    /// The cluster-minted entity id from the seeder's manifest.
    pub entity: u64,
    /// The interest cell the seeder committed the row to.
    pub cell: u64,
    /// The gateway whose shard set covers `cell`.
    pub side: Side,
}

/// What one peer holds for one entity.
#[derive(Debug, Clone, Copy)]
struct Held {
    lease_id: LeaseId,
    seq: SeqPair,
    /// The cell to address a diff to, when this peer knows one.
    ///
    /// `None` for a lease inherited over an entity outside this peer's own
    /// interest zone. `LeaseMsg::Grant` carries `claim_id, entity, lease_id,
    /// seq, ttl_ms, prev_holder` and **no cell**, so an inherited row's cell
    /// can only come from the harness's own inventory — and a registrar is
    /// free to pick a successor this harness did not predict. Such a lease is
    /// still *held*: it is renewed on the heartbeat, which needs only the
    /// entity and the token. It is only the bulk uplink that needs an address.
    ///
    /// Recording it rather than dropping it is load-bearing. Dropping one made
    /// the first run of this gate report 1 reassignment against 13 the
    /// registrars had attested, and read as a 12-row settle failure.
    cell: Option<CellId>,
    tick: u64,
}

/// A line in a peer's event log, read back by the orchestrator.
///
/// Every variant carries `side`, because in this topology "an entity's lease
/// ended" is only half a fact: which registrar ended it is the other half, and
/// it is the half the gateway-kill clause is entirely about.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PeerEvent {
    /// The peer's own claim was granted.
    Claimed {
        /// The entity claimed.
        entity: u64,
        /// The granted lease token.
        lease_id: u64,
        /// The gateway that granted it.
        side: Side,
    },
    /// The peer's own claim was refused.
    Denied {
        /// The entity claimed.
        entity: u64,
        /// The refusal, as the registrar spelled it. `WrongOwner` here means
        /// the harness addressed the wrong gateway (#117); anything else is a
        /// refusal about the claim rather than about the address.
        reason: String,
        /// The gateway that refused.
        side: Side,
    },
    /// The registrar handed this peer a lease it never asked for.
    Inherited {
        /// The entity inherited.
        entity: u64,
        /// The new lease token.
        lease_id: u64,
        /// Wall-clock instant the grant arrived, which is what stops the
        /// orchestrator's clock for a reassigned entity.
        at_ms: u64,
        /// The gateway that reassigned it.
        side: Side,
    },
    /// A held lease ended.
    Lost {
        /// The entity whose lease ended.
        entity: u64,
        /// The registrar's disposition.
        disposition: String,
        /// The gateway that withdrew it.
        side: Side,
        /// Wall-clock instant the withdrawal arrived. The gateway-kill clause
        /// is a statement about *when* a lease ended relative to a SIGKILL on
        /// the other process, so the bare fact is not enough.
        at_ms: u64,
    },
    /// The peer stopped talking to one gateway because its connection is gone.
    ///
    /// Expected exactly once, for the gateway the orchestrator kills. Logged
    /// rather than inferred so a run in which the *wrong* gateway died is a
    /// visible fact instead of a puzzling silence.
    GatewayGone {
        /// The gateway whose connection closed.
        side: Side,
        /// Wall-clock instant the peer noticed.
        at_ms: u64,
    },
    /// The peer finished its run cleanly.
    Done {
        /// Leases still held on gateway A.
        held_a: usize,
        /// Leases still held on gateway B.
        held_b: usize,
    },
}

/// Everything one peer process needs, handed over as a file rather than argv.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct PeerSpec {
    /// Gateway A's `bind_addr` from its readiness line.
    pub gateway_a_addr: String,
    /// Gateway A's `node_id` from its readiness line.
    pub gateway_a_node: String,
    /// Gateway B's `bind_addr`.
    pub gateway_b_addr: String,
    /// Gateway B's `node_id`.
    pub gateway_b_node: String,
    /// The coordinator's `bind_addr`.
    pub coordinator_addr: String,
    /// The coordinator's `node_id`.
    pub coordinator_node: String,
    /// This peer's hex-encoded iroh secret key.
    pub secret: String,
    /// This peer's hex-encoded session token.
    pub token: String,
    /// Every row in this peer's interest zone, whether or not this peer
    /// claims it.
    ///
    /// Two jobs, and they are why the zone is carried rather than just the
    /// claimed set. The distinct cells are what the peer reports presence
    /// over, and overlapping zones are what make a successor exist at all:
    /// the registrar picks one from the peers with coordinator-confirmed
    /// interest in the lost lease's cell. And a `Grant` carries no cell
    /// (`LeaseMsg::Grant` is `claim_id, entity, lease_id, seq, ttl_ms,
    /// prev_holder`), so an inherited row's cell has to be looked up here or
    /// the peer holds a lease it cannot address a diff to — see [`Held::cell`],
    /// which keeps it either way.
    pub zone_rows: Vec<Row>,
    /// The subset of `zone_rows` this peer claims, each routed to its owner.
    pub rows: Vec<Row>,
    /// The tier this peer claims at.
    pub kind: ClaimKind,
    /// How long the peer keeps simulating.
    pub duration_secs: u64,
    /// Where the peer writes its JSONL event log.
    pub log: std::path::PathBuf,
}

/// Wall-clock milliseconds, matching the orchestrator's own stamp.
fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

/// One gateway's half of a peer: a session and the leases it holds there.
struct Leg {
    side: Side,
    session: Session,
    held: BTreeMap<PersistId, Held>,
    /// Set once the connection is observed closed; the peer then stops
    /// selecting on it rather than spinning on an instantly-`None` read.
    gone: bool,
}

/// Run one peer until its duration elapses, or forever if killed first.
pub async fn run(spec: PeerSpec) -> Result<()> {
    let mut log = std::fs::File::create(&spec.log)
        .with_context(|| format!("create peer log {}", spec.log.display()))?;
    let mut emit = move |event: &PeerEvent| {
        if let Ok(line) = serde_json::to_string(event) {
            let _ = writeln!(log, "{line}");
            let _ = log.flush();
        }
    };

    let secret = iroh::SecretKey::from_bytes(&crate::decode_key(&spec.secret)?);
    let token = crate::decode_hex(&spec.token)?;
    let node = secret.public();
    // Entity → cell for the whole zone, so an inherited lease can be uplinked
    // for: the grant that carries it names no cell.
    let mut cell_of: BTreeMap<PersistId, CellId> = BTreeMap::new();
    for row in &spec.zone_rows {
        let cell = CellId::from_bits(row.cell).context("zone row cell is not a valid CellId")?;
        cell_of.insert(PersistId::new(row.entity), cell);
    }
    let interest: Vec<CellId> = cell_of
        .values()
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    // Interest comes from the coordinator, not from the harness, and there is
    // exactly one grant: two gateways in one grid are two shard sets of one
    // interest space, so a peer that needed a grant per gateway would be
    // describing a topology this one is not.
    let coordinator = orrery_coordinator::CoordinatorClient::connect(
        secret.clone(),
        crate::endpoint_addr(&spec.coordinator_node, &spec.coordinator_addr)?,
        token.clone(),
        Duration::from_secs(10),
    )
    .await
    .map_err(|error| anyhow::anyhow!("coordinator session: {error}"))?;
    coordinator
        .report_presence(interest)
        .map_err(|error| anyhow::anyhow!("report presence: {error}"))?;
    let grant = coordinator
        .next_grant(Duration::from_secs(15))
        .await
        .map_err(|error| anyhow::anyhow!("interest grant: {error}"))?;

    let mut legs = Vec::new();
    for (side, node_id, addr) in [
        (Side::A, &spec.gateway_a_node, &spec.gateway_a_addr),
        (Side::B, &spec.gateway_b_node, &spec.gateway_b_addr),
    ] {
        let session =
            Session::connect(secret.clone(), crate::endpoint_addr(node_id, addr)?).await?;
        session.send_control(&GatewayMsg::Hello {
            token: token.clone(),
            node,
        })?;
        let hello = session.recv(Duration::from_secs(10)).await;
        anyhow::ensure!(
            matches!(hello, Some(GatewayReply::HelloAck { .. })),
            "gateway {side:?} did not accept the peer's hello: {hello:?}"
        );
        session.send_control(&GatewayMsg::InterestGrant {
            grant: grant.clone(),
        })?;
        match session.recv(Duration::from_secs(10)).await {
            Some(GatewayReply::InterestAck {
                epoch: Some(_),
                reason: _,
            }) => {}
            other => anyhow::bail!("gateway {side:?} refused the peer's interest grant: {other:?}"),
        }
        legs.push(Leg {
            side,
            session,
            held: BTreeMap::new(),
            gone: false,
        });
    }

    // Claims, each addressed to the gateway that owns the cell. A claim sent
    // to the sibling would come back `WrongOwner` (#117) rather than silently
    // failing, which is why a misrouted harness is a diagnosable one.
    let mut claim_id = 0u64;
    for row in &spec.rows {
        claim_id += 1;
        let entity = PersistId::new(row.entity);
        let cell = CellId::from_bits(row.cell).context("row cell is not a valid CellId")?;
        let leg = legs
            .iter_mut()
            .find(|leg| leg.side == row.side)
            .expect("both sides are present");
        leg.session.send_control(&GatewayMsg::Lease {
            message: LeaseMsg::Claim {
                claim_id: ClaimId(claim_id),
                entity,
                grid: GridId::ROOT,
                cell,
                kind: spec.kind,
                basis: ClaimBasis::Contact { tick: Tick::new(0) },
                observed: SeqPair::default(),
                tick: Tick::new(0),
            },
        })?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let Some(reply) = leg.session.recv(remaining).await else {
                anyhow::bail!(
                    "claim for {entity:?} on gateway {:?} went unanswered",
                    leg.side
                );
            };
            match reply {
                GatewayReply::Lease {
                    message:
                        LeaseMsg::Grant {
                            claim_id: answered,
                            entity: granted,
                            lease_id,
                            seq,
                            ..
                        },
                } if answered == ClaimId(claim_id) && granted == entity => {
                    leg.held.insert(
                        entity,
                        Held {
                            lease_id,
                            seq,
                            cell: Some(cell),
                            tick: 1,
                        },
                    );
                    emit(&PeerEvent::Claimed {
                        entity: entity.0,
                        lease_id: lease_id.0,
                        side: leg.side,
                    });
                    break;
                }
                GatewayReply::Lease {
                    message:
                        LeaseMsg::Deny {
                            claim_id: Some(answered),
                            entity: denied,
                            reason,
                            ..
                        },
                } if answered == ClaimId(claim_id) && denied == entity => {
                    emit(&PeerEvent::Denied {
                        entity: entity.0,
                        reason: format!("{reason:?}"),
                        side: leg.side,
                    });
                    break;
                }
                other => apply_unsolicited(leg.side, other, &cell_of, &mut leg.held, &mut emit),
            }
        }
    }

    let started = tokio::time::Instant::now();
    let duration = Duration::from_secs(spec.duration_secs);
    let mut uplink = tokio::time::interval(UPLINK_INTERVAL);
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.tick().await;

    while started.elapsed() < duration {
        tokio::select! {
            _ = uplink.tick() => {
                for leg in legs.iter_mut().filter(|leg| !leg.gone) {
                    for (entity, state) in leg.held.iter_mut() {
                        let Some(cell) = state.cell else {
                            continue;
                        };
                        state.tick += 1;
                        let payload = bytes::Bytes::from(state.tick.to_le_bytes().to_vec());
                        // A send to a gateway that has just died fails; that is
                        // the event the loop is waiting to observe, not an
                        // error to abort on.
                        let _ = leg.session.send_state(&GatewayMsg::Diff {
                            diff: orrery_protocol::DiffUplink {
                                cell,
                                grid: GridId::ROOT,
                                entity: *entity,
                                tick: Tick::new(state.tick),
                                kind: RecordKind::ComponentDiff,
                                payload,
                                seq: state.tick,
                                lease_id: Some(state.lease_id),
                                authority_seq: Some(state.seq),
                            },
                        });
                    }
                }
            }
            _ = heartbeat.tick() => {
                for leg in legs.iter_mut().filter(|leg| !leg.gone) {
                    if leg.held.is_empty() {
                        continue;
                    }
                    let _ = leg.session.send_control(&GatewayMsg::Lease {
                        message: LeaseMsg::Heartbeat {
                            renew: leg
                                .held
                                .iter()
                                .map(|(entity, state)| (*entity, state.lease_id))
                                .collect(),
                            tick: Tick::new(started.elapsed().as_millis() as u64),
                        },
                    });
                }
            }
            (side, reply) = next_reply(&mut legs) => {
                if let Some(reply) = reply {
                    if let Some(leg) = legs.iter_mut().find(|leg| leg.side == side) {
                        apply_unsolicited(side, reply, &cell_of, &mut leg.held, &mut emit);
                    }
                } else if let Some(leg) = legs.iter_mut().find(|leg| leg.side == side) {
                    // A read that came back empty on a connection the
                    // transport has closed is the sibling being gone. A read
                    // that came back empty on a live connection is just an
                    // idle interval.
                    if leg.session.connection.close_reason().is_some() {
                        leg.gone = true;
                        emit(&PeerEvent::GatewayGone { side, at_ms: unix_ms() });
                    }
                }
            }
        }
    }

    let held_a = legs
        .iter()
        .find(|leg| leg.side == Side::A)
        .map_or(0, |leg| leg.held.len());
    let held_b = legs
        .iter()
        .find(|leg| leg.side == Side::B)
        .map_or(0, |leg| leg.held.len());
    emit(&PeerEvent::Done { held_a, held_b });
    drop(coordinator);
    Ok(())
}

/// The next reply from whichever leg speaks first.
///
/// Both legs are polled together rather than in turn, because a peer that
/// round-robins with a timeout on each would delay an inherited grant on one
/// gateway by the other's idle interval — and that delay would land directly
/// in the settle time this harness reports.
async fn next_reply(legs: &mut [Leg]) -> (Side, Option<GatewayReply>) {
    let poll = Duration::from_millis(100);
    let (first, rest) = legs.split_at(1);
    let a = &first[0];
    let b = &rest[0];
    if a.gone {
        return (b.side, b.session.recv(poll).await);
    }
    if b.gone {
        return (a.side, a.session.recv(poll).await);
    }
    tokio::select! {
        reply = a.session.recv(poll) => (a.side, reply),
        reply = b.session.recv(poll) => (b.side, reply),
    }
}

/// Fold a reply the peer did not ask for into one leg's held set.
fn apply_unsolicited(
    side: Side,
    reply: GatewayReply,
    cell_of: &BTreeMap<PersistId, CellId>,
    held: &mut BTreeMap<PersistId, Held>,
    emit: &mut impl FnMut(&PeerEvent),
) {
    match reply {
        GatewayReply::Lease {
            message:
                LeaseMsg::Grant {
                    claim_id,
                    entity,
                    lease_id,
                    seq,
                    ..
                },
        } if claim_id == ClaimId::REGISTRAR => {
            let advances = held
                .get(&entity)
                .is_none_or(|current| lease_id > current.lease_id);
            // An inherited entity outside this peer's zone has no address here
            // — a `Grant` names no cell — so it is held without one and simply
            // not uplinked for. It is still renewed, and it is still reported.
            let cell = cell_of.get(&entity).copied();
            if advances {
                held.insert(
                    entity,
                    Held {
                        lease_id,
                        seq,
                        cell,
                        tick: 1,
                    },
                );
                emit(&PeerEvent::Inherited {
                    entity: entity.0,
                    lease_id: lease_id.0,
                    at_ms: unix_ms(),
                    side,
                });
            }
        }
        GatewayReply::Lease {
            message:
                LeaseMsg::Expire {
                    entity,
                    lease_id,
                    disposition,
                    ..
                },
        } => {
            if held
                .get(&entity)
                .is_some_and(|current| current.lease_id == lease_id)
            {
                held.remove(&entity);
                emit(&PeerEvent::Lost {
                    entity: entity.0,
                    disposition: format!("{disposition:?}"),
                    side,
                    at_ms: unix_ms(),
                });
            }
        }
        GatewayReply::BulkNack {
            entity,
            lease: Some(_),
            ..
        } => {
            held.remove(&entity);
        }
        _ => {}
    }
}
