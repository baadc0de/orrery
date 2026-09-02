//! Orrery peer sessions bridged into Lightyear's P2P replication stack.
//!
//! `orrery_net` deliberately owns the concrete Aeronet session queues, the
//! Orrery channel tag, and upload-budget enforcement. Attaching upstream's
//! generic `lightyear_aeronet` adapter directly would give two systems the
//! same receive queue and would let Lightyear packets bypass that policy.
//! This bridge therefore meets the transport at `PeerPacket`/`SendPacket`:
//! Aeronet types stay in `orrery_net`, and every Lightyear type stays here.

use std::time::Duration;

use bevy_app::{App, Plugin, PostUpdate, PreUpdate, Startup, Update};
use bevy_ecs::prelude::*;
use bytes::BytesMut;
use lightyear::prelude::server::{ClientOf, ServerPlugins, Started};
use lightyear::prelude::{
    Connected, Link, LinkMtu, LinkSystems, Linked, PeerId, PeerMetadata, RemoteId,
    ReplicationReceiver, ReplicationSender, P2P,
};
use lightyear_replication::channels::RepliconChannelMap;
use orrery_net::channels::{tag, untag, Channel, TAG_REPLICATION, TAG_WITNESS_KEYFRAME};
use orrery_net::peer_link::{payload_budget, PeerPacket, SendPacket};
use orrery_net::plugin::{PeerMtu, PeerRegistry};
use orrery_protocol::NodeId;

/// A Lightyear link created from one established Orrery peer session.
#[derive(Debug, Clone, Copy, Component)]
pub struct ReplicationPeerLink {
    /// Aeronet session entity tracked by `orrery_net`.
    pub session: Entity,
    /// Authenticated iroh identity at the other end of the session.
    pub peer: NodeId,
}

/// Installs the P2P sender half and bridges Orrery peer packets to Lightyear.
///
/// [`ClientPlugins`] is already installed by `OrreryPredictPlugin`; the
/// server group contributes Replicon's sender backend. No conventional
/// Lightyear server entity is spawned: each established session is one direct
/// [`P2P`] link that is both a replication receiver and sender.
#[derive(Debug, Clone, Copy)]
pub struct OrreryReplicationBridgePlugin {
    /// The same fixed tick duration used by `OrreryPredictPlugin`.
    pub tick_duration: Duration,
}

impl Plugin for OrreryReplicationBridgePlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(ServerPlugins {
            tick_duration: self.tick_duration,
        });
        // Orrery supplies an already-authenticated connection instead of one
        // of Lightyear's connection plugins, so it must seed the metadata
        // resource their lifecycle hooks normally install.
        app.init_resource::<PeerMetadata>();
        app.add_systems(Startup, align_p2p_replicon_channels);
        app.add_systems(Update, synchronize_peer_links);
        app.add_systems(
            PreUpdate,
            receive_replication_packets.before(LinkSystems::Receive),
        );
        app.add_systems(
            PostUpdate,
            send_replication_packets.in_set(LinkSystems::Send),
        );
    }
}

/// Match Lightyear's map to Replicon's actual directional channel counts.
///
/// Lightyear 0.29 builds a three-entry map for both namespaces, anticipating
/// fully bidirectional Replicon replication. The pinned Replicon backend has
/// two server channels (updates, mutations) and one client channel (mutation
/// acknowledgements). A combined P2P role runs both packet bridges, so leaving
/// the speculative entries present makes the server bridge index receive
/// channels that do not exist.
fn align_p2p_replicon_channels(mut channels: ResMut<RepliconChannelMap>) {
    channels.server_channels.truncate(2);
    channels.client_channels.truncate(1);
}

/// Owns the `Started` lifecycle marker that enables Replicon's sender schedule
/// without declaring a conventional Lightyear `Server` topology.
#[derive(Component)]
struct P2PSenderSchedule;

fn peer_id(node: NodeId) -> PeerId {
    let bytes = node.as_bytes();
    let mut folded = [0_u8; 8];
    for (index, byte) in bytes.iter().enumerate() {
        folded[index % folded.len()] ^= byte;
    }
    PeerId::Entity(u64::from_le_bytes(folded))
}

fn synchronize_peer_links(
    mut commands: Commands,
    peers: Res<PeerRegistry>,
    session_mtus: Query<&PeerMtu>,
    links: Query<(Entity, &ReplicationPeerLink)>,
    sender_schedule: Query<Entity, With<P2PSenderSchedule>>,
) {
    for (session, peer) in &peers.peers {
        if links.iter().any(|(_, link)| link.session == *session) {
            continue;
        }

        let Ok(session_mtu) = session_mtus.get(*session) else {
            // `PeerRegistry` is updated in the same deferred batch that adds
            // `PeerMtu`; the component is visible on the next update.
            continue;
        };
        let mtu = payload_budget(session_mtu.0);
        let link = commands
            .spawn((
                Link::default().with_mtu(LinkMtu::new(mtu)),
                RemoteId(peer_id(peer.id)),
                P2P,
                ClientOf,
                ReplicationSender,
                ReplicationReceiver,
                ReplicationPeerLink {
                    session: *session,
                    peer: peer.id,
                },
            ))
            .id();
        // Lifecycle hooks require RemoteId and the role markers to exist first.
        commands.entity(link).insert((Linked, Connected));
    }

    for (entity, link) in &links {
        if !peers
            .peers
            .iter()
            .any(|(session, _)| *session == link.session)
        {
            commands.entity(entity).try_despawn();
        }
    }

    if peers.is_empty() {
        for entity in &sender_schedule {
            commands.entity(entity).try_despawn();
        }
    } else if sender_schedule.is_empty() {
        // Replicon still names its sender schedule "server" even for direct
        // P2P senders. `Started` drives that schedule, while the absence of a
        // `Server` component keeps Lightyear's topology correctly classified
        // as P2P so prediction remains enabled.
        commands.spawn((P2PSenderSchedule, Started));
    }
}

fn receive_replication_packets(
    mut packets: MessageReader<PeerPacket>,
    mut links: Query<(&ReplicationPeerLink, &mut Link)>,
) {
    for packet in packets.read() {
        if packet.channel != Channel::State {
            continue;
        }
        // Replication shares State with witness and hit traffic. The peer-link
        // boundary has removed its outer channel tag, but the logical payload
        // still carries the protocol envelope that every State consumer uses.
        // Do not let a foreign sub-tag reach Lightyear's packet parser.
        let Some((Channel::State, payload)) = untag(&packet.payload) else {
            continue;
        };
        let Some((sub_tag, payload)) = payload.split_first() else {
            continue;
        };
        if *sub_tag != TAG_REPLICATION && *sub_tag != TAG_WITNESS_KEYFRAME {
            continue;
        }
        let Some((_, mut link)) = links.iter_mut().find(|(link, _)| link.peer == packet.from)
        else {
            continue;
        };
        link.recv.push_raw(BytesMut::from(payload));
    }
}

fn send_replication_packets(
    mut links: Query<(&ReplicationPeerLink, &mut Link), With<Linked>>,
    mut packets: MessageWriter<SendPacket>,
) {
    for (peer, mut link) in &mut links {
        for payload in link.send.drain() {
            let mut replication = Vec::with_capacity(payload.len() + 1);
            replication.push(TAG_REPLICATION);
            replication.extend_from_slice(&payload);
            packets.write(SendPacket::state(
                peer.peer,
                tag(Channel::State, &replication).into(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use bevy_ecs::{message::Messages, prelude::Entity, system::RunSystemOnce, world::World};
    use bytes::Bytes;
    use lightyear::prelude::Linked;
    use orrery_net::{
        channels::{
            decode_hit, decode_replication, decode_witness, encode_hit, encode_witness, tag, untag,
            Channel, TAG_REPLICATION, TAG_WITNESS_KEYFRAME,
        },
        PeerPacket, SendPacket,
    };
    use orrery_protocol::{
        HitClaim, HitMsg, HitSurface, InterpBasis, LatticePoint, NodeId, PersistId, QuantizedDir,
        QuantizedRay, Tick, WeaponRef,
    };

    use super::{receive_replication_packets, send_replication_packets, ReplicationPeerLink};
    use lightyear::prelude::Link;

    fn node() -> NodeId {
        NodeId::from_bytes(&[0; 32]).expect("test NodeId")
    }

    fn hit_frame() -> Vec<u8> {
        encode_hit(&HitMsg::Claim(HitClaim {
            shooter: PersistId::new(1),
            target: PersistId::new(2),
            weapon: WeaponRef(1),
            fire_tick: Tick::new(100),
            basis: InterpBasis::exact(Tick::new(95)),
            ray: QuantizedRay {
                origin: LatticePoint::new(0, 0, 0),
                direction: QuantizedDir::new(1, 0, 0),
            },
            claimed: HitSurface(0),
            input_seq: 1,
        }))
    }

    #[test]
    fn only_replication_state_packets_reach_lightyears_parser() {
        let peer = node();
        let mut world = World::new();
        world.init_resource::<Messages<PeerPacket>>();
        let link = world
            .spawn((
                Link::default(),
                ReplicationPeerLink {
                    session: Entity::PLACEHOLDER,
                    peer,
                },
            ))
            .id();

        {
            let mut packets = world.resource_mut::<Messages<PeerPacket>>();
            // All three State-channel participants arrive in one pass. Only
            // the replication body is valid input for Lightyear's parser.
            for payload in [
                encode_witness(&1u8),
                hit_frame(),
                tag(Channel::State, &[TAG_REPLICATION, 3, 4]),
                tag(Channel::State, &[TAG_WITNESS_KEYFRAME, 5, 6]),
            ] {
                packets.write(PeerPacket {
                    from: peer,
                    channel: Channel::State,
                    payload: Bytes::from(payload),
                });
            }
        }

        world.run_system_once(receive_replication_packets).unwrap();

        let mut parser = world.get_mut::<Link>(link).expect("bridge link");
        assert_eq!(
            parser.recv.len(),
            2,
            "only the two replication families reach Lightyear's parser"
        );
        assert_eq!(parser.recv.pop().as_deref(), Some(&[3, 4][..]));
        assert_eq!(parser.recv.pop().as_deref(), Some(&[5, 6][..]));
    }

    #[test]
    fn outbound_bridge_frames_are_replication_tagged_and_foreign_decoders_reject_them() {
        let peer = node();
        let mut world = World::new();
        world.init_resource::<Messages<SendPacket>>();
        let mut parser = Link::default();
        parser.send.push(Bytes::from_static(&[42]));
        world.spawn((
            parser,
            Linked,
            ReplicationPeerLink {
                session: Entity::PLACEHOLDER,
                peer,
            },
        ));

        world.run_system_once(send_replication_packets).unwrap();

        let sent = world
            .resource_mut::<Messages<SendPacket>>()
            .drain()
            .collect::<Vec<_>>();
        assert_eq!(sent.len(), 1);
        let frame = &sent[0];
        assert_eq!(frame.channel, Channel::State);
        let (_, payload) = untag(&frame.payload).expect("logical state envelope");
        assert_eq!(payload.first(), Some(&TAG_REPLICATION));
        assert_eq!(decode_replication::<u8>(&frame.payload), Some(42));
        assert_eq!(decode_witness::<u8>(&frame.payload), None);
        assert_eq!(decode_hit(&frame.payload), None);
    }
}
