//! One island peer, as its own OS process.
//!
//! Separate processes are the point: the demo criterion says `kill -9`, and a
//! dropped task would prove only that the harness can stop calling the
//! gateway. A SIGKILLed process leaves the gateway to notice a torn QUIC
//! connection exactly as it would in production.

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

/// What one peer holds for one entity.
#[derive(Debug, Clone, Copy)]
struct Held {
    lease_id: LeaseId,
    seq: SeqPair,
    tick: u64,
}

/// A line in a peer's event log, read back by the orchestrator.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PeerEvent {
    /// The peer's own claim was granted.
    Claimed { entity: u64, lease_id: u64 },
    /// The peer's own claim was refused.
    Denied { entity: u64, reason: String },
    /// The registrar handed this peer a lease it never asked for.
    Inherited { entity: u64, lease_id: u64 },
    /// A held lease ended.
    Lost { entity: u64, disposition: String },
    /// The peer finished its run cleanly.
    Done { held: usize },
}

/// Everything one peer process needs, passed on argv by the orchestrator.
pub struct PeerConfig {
    pub gateway: iroh::EndpointAddr,
    pub secret: iroh::SecretKey,
    pub token: Vec<u8>,
    pub grant: Vec<u8>,
    pub cell: CellId,
    pub entities: Vec<u64>,
    pub duration: Duration,
    pub log: std::path::PathBuf,
}

/// Run one peer until its duration elapses, or forever if killed first.
pub async fn run(config: PeerConfig) -> Result<()> {
    let mut log = std::fs::File::create(&config.log)
        .with_context(|| format!("create peer log {}", config.log.display()))?;
    let mut emit = move |event: &PeerEvent| {
        if let Ok(line) = serde_json::to_string(event) {
            let _ = writeln!(log, "{line}");
            let _ = log.flush();
        }
    };

    let session = Session::connect(config.secret.clone(), config.gateway).await?;
    let node = config.secret.public();

    session.send_control(&GatewayMsg::Hello {
        token: config.token,
        node,
    })?;
    let hello = session.recv(Duration::from_secs(10)).await;
    anyhow::ensure!(
        matches!(hello, Some(GatewayReply::HelloAck { .. })),
        "gateway did not accept the peer's hello: {hello:?}"
    );

    // Interest before any claim: a claim judged against interest the gateway
    // has not seen yet is refused, and that would look like a registrar bug.
    session.send_control(&GatewayMsg::InterestGrant {
        grant: config.grant,
    })?;
    let ack = session.recv(Duration::from_secs(10)).await;
    match ack {
        Some(GatewayReply::InterestAck {
            epoch: Some(_),
            reason: _,
        }) => {}
        other => anyhow::bail!("gateway refused the peer's interest grant: {other:?}"),
    }

    let mut held: std::collections::BTreeMap<PersistId, Held> = std::collections::BTreeMap::new();
    for (index, entity) in config.entities.iter().enumerate() {
        let entity = PersistId::new(*entity);
        let claim_id = ClaimId(index as u64 + 1);
        session.send_control(&GatewayMsg::Lease {
            message: LeaseMsg::Claim {
                claim_id,
                entity,
                grid: GridId::ROOT,
                cell: config.cell,
                kind: ClaimKind::Weak,
                basis: ClaimBasis::Contact { tick: Tick::new(0) },
                observed: SeqPair::default(),
                tick: Tick::new(0),
            },
        })?;
        // Claims are answered on the same lane, so drain until this one lands
        // rather than assuming an ordering the protocol does not promise.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let Some(reply) = session.recv(remaining).await else {
                anyhow::bail!("claim for {entity:?} went unanswered");
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
                } if answered == claim_id && granted == entity => {
                    held.insert(
                        entity,
                        Held {
                            lease_id,
                            seq,
                            tick: 1,
                        },
                    );
                    emit(&PeerEvent::Claimed {
                        entity: entity.0,
                        lease_id: lease_id.0,
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
                } if answered == claim_id && denied == entity => {
                    emit(&PeerEvent::Denied {
                        entity: entity.0,
                        reason: format!("{reason:?}"),
                    });
                    break;
                }
                other => apply_unsolicited(other, &mut held, &mut emit),
            }
        }
    }

    let started = tokio::time::Instant::now();
    let mut uplink = tokio::time::interval(UPLINK_INTERVAL);
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    // The first tick of a tokio interval fires immediately; the heartbeat
    // should not race the claims that just landed.
    heartbeat.tick().await;

    loop {
        if started.elapsed() >= config.duration {
            break;
        }
        tokio::select! {
            _ = uplink.tick() => {
                for (entity, state) in held.iter_mut() {
                    state.tick += 1;
                    let payload = bytes::Bytes::from(state.tick.to_le_bytes().to_vec());
                    session.send_state(&GatewayMsg::Diff {
                        diff: orrery_protocol::DiffUplink {
                            cell: config.cell,
                            grid: GridId::ROOT,
                            entity: *entity,
                            tick: Tick::new(state.tick),
                            kind: RecordKind::ComponentDiff,
                            payload,
                            seq: state.tick,
                            lease_id: Some(state.lease_id),
                            authority_seq: Some(state.seq),
                        },
                    })?;
                }
            }
            _ = heartbeat.tick() => {
                if !held.is_empty() {
                    session.send_control(&GatewayMsg::Lease {
                        message: LeaseMsg::Heartbeat {
                            lease_ids: held.values().map(|state| state.lease_id).collect(),
                            tick: Tick::new(started.elapsed().as_millis() as u64),
                        },
                    })?;
                }
            }
            reply = session.recv(Duration::from_millis(100)) => {
                if let Some(reply) = reply {
                    apply_unsolicited(reply, &mut held, &mut emit);
                }
            }
        }
    }

    emit(&PeerEvent::Done { held: held.len() });
    Ok(())
}

/// Fold a reply the peer did not ask for into its held set.
///
/// This is the client half of redistribution: an inherited grant arrives with
/// the reserved registrar correlation and no pending claim behind it, and an
/// expiry names the token the peer still believes it holds.
fn apply_unsolicited(
    reply: GatewayReply,
    held: &mut std::collections::BTreeMap<PersistId, Held>,
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
            if advances {
                held.insert(
                    entity,
                    Held {
                        lease_id,
                        seq,
                        tick: 1,
                    },
                );
                emit(&PeerEvent::Inherited {
                    entity: entity.0,
                    lease_id: lease_id.0,
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
                });
            }
        }
        GatewayReply::BulkNack {
            entity,
            lease: Some(_),
            ..
        } => {
            // A lease-bearing nack is the fence saying this peer is no longer
            // the writer. Stop immediately rather than wait for a local expiry
            // estimate — that is the whole point of fencing on the token.
            held.remove(&entity);
        }
        _ => {}
    }
}
