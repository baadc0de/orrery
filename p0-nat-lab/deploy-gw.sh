#!/usr/bin/env bash
# Deploy a fake-NAT gateway for the P0 NAT lab.
#
# Each gateway hosts N peer namespaces behind an iptables NAT that emulates a
# chosen NAT type. Peers have NO external IP of their own; they route through
# the gateway's external IP via a lab bridge, exactly like a LAN box behind a
# home router.
#
# NAT types (per docs/11-roadmap.md P0):
#   fullcone       - plain SNAT/masquerade, endpoint-independent mapping+filter
#   portrestricted - masquerade + only allow return traffic to the exact
#                    (srcIP,srcPort) the peer contacted (conntrack ESTABLISHED)
#   symmetric      - per-destination source-port mapping approximation
#   udpblocked     - drop all UDP egress -> peers are forced onto the relay
#
# Usage: sudo ./deploy-gw.sh <nat-type> <peer1> [peer2 ...]
#   e.g. sudo ./deploy-gw.sh fullcone p1 p2
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
RELAY_HOST_FILE="$SCRIPT_DIR/../p0-nat-test/relay-host"

# Keep the relay's host in one checked-in file. Both the CLI and this deployer
# honour ORRERY_RELAY_HOST, so a temporary relay can be selected without
# editing it. The lab still pins an IP below: peer network namespaces cannot
# rely on DNS once they are isolated behind the fake NAT.
relay_host() {
  local default_host
  default_host=$(<"$RELAY_HOST_FILE")
  printf '%s\n' "${ORRERY_RELAY_HOST:-$default_host}"
}

RELAY_HOST=$(relay_host)
if [ "${1:-}" = "--print-relay-host" ]; then
  printf '%s\n' "$RELAY_HOST"
  exit 0
fi

NAT_TYPE="${1:?usage: deploy-gw.sh <nat-type> <peer...>}"
shift
PEERS=("$@")
[ "${#PEERS[@]}" -ge 1 ] || { echo "need >=1 peer name"; exit 1; }

# The gateway's external NIC (the one with the public IP / default route).
EXT_IF=$(ip -4 route show default | awk '{print $5; exit}')
echo "gateway: ext_if=$EXT_IF nat_type=$NAT_TYPE peers=${PEERS[*]}"

RELAY_IP=$(getent ahostsv4 "$RELAY_HOST" | awk 'NR == 1 { print $1 }')
[ -n "$RELAY_IP" ] || { echo "could not resolve relay host: $RELAY_HOST" >&2; exit 1; }

sysctl -w net.ipv4.ip_forward=1 >/dev/null

# --- flush any prior lab state (idempotent re-run) ---
for p in "${PEERS[@]}"; do
  ip netns del "$p" 2>/dev/null || true
  ip link del "veth-${p}" 2>/dev/null || true
  ip link del "veth-${p}-ns" 2>/dev/null || true
done
iptables -t nat -F 2>/dev/null || true
iptables -F FORWARD 2>/dev/null || true
iptables -t nat -X SYMMETRIC 2>/dev/null || true
ip link del br-lab 2>/dev/null || true
ip route del 10.200.0.0/24 2>/dev/null || true

# --- build a lab bridge for all peer namespaces ---
# All peer host-side veths join one bridge (br-lab) so the peers share one L2
# segment; the gateway owns 10.200.0.1/24 on the bridge and does NAT on egress
# via EXT_IF. Avoids the /24 duplication that broke multi-peer ARP with
# per-veth subnets.
BRIDGE="br-lab"
ip link add "$BRIDGE" type bridge 2>/dev/null || true
ip link set "$BRIDGE" up
ip addr flush dev "$BRIDGE" 2>/dev/null || true
ip addr add 10.200.0.1/24 dev "$BRIDGE" 2>/dev/null || true
ip route add 10.200.0.0/24 dev "$BRIDGE" 2>/dev/null || true

idx=0
for p in "${PEERS[@]}"; do
  idx=$((idx+1))
  veth_host="veth-${p}"
  veth_ns="veth-${p}-ns"
  peer_ip="10.200.0.$((100+idx))"

  ip netns add "$p"
  ip link add "$veth_host" type veth peer name "$veth_ns"
  ip link set "$veth_ns" netns "$p"
  ip link set "$veth_host" up
  ip link set "$veth_host" master "$BRIDGE"

  ip netns exec "$p" ip link set lo up
  ip netns exec "$p" ip link set "$veth_ns" name eth0
  ip netns exec "$p" ip addr add "$peer_ip/24" dev eth0
  ip netns exec "$p" ip link set eth0 up
  ip netns exec "$p" ip route add default via 10.200.0.1 dev eth0

  # Working resolver for the peer (systemd-resolved in the gateway ns is not
  # reachable from the peer ns). Resolve the shared relay host before entering
  # the namespace, then pin its IP: iroh's async resolver cannot rely on DNS
  # through this fake NAT.
  ip netns exec "$p" bash -c 'echo "nameserver 8.8.8.8
nameserver 1.1.1.1" > /etc/resolv.conf'
  ip netns exec "$p" bash -c 'printf "%s %s\\n" "$1" "$2" >> /etc/hosts' \
    relay-host-pin "$RELAY_IP" "$RELAY_HOST"
  echo "peer $p: ip=$peer_ip on bridge $BRIDGE (gw 10.200.0.1)"
done

# --- NAT rules per type ---
# Peers live on the br-lab bridge (10.200.0.0/24, gw = 10.200.0.1). Forwarded
# traffic egresses via $EXT_IF and is NAT'd there. Inbound reply traffic
# arrives on $EXT_IF and is un-NAT'd back to the peer's bridge IP.
BR_IF="$BRIDGE"
case "$NAT_TYPE" in
  fullcone)
    # Endpoint-independent mapping + filter: plain masquerade. Any external
    # host can send into the mapping once it exists.
    iptables -t nat -A POSTROUTING -o "$EXT_IF" -j MASQUERADE
    iptables -A FORWARD -i "$BR_IF" -j ACCEPT
    iptables -A FORWARD -j ACCEPT
    ;;
  portrestricted)
    # Masquerade + only allow return traffic to the exact endpoint the peer
    # contacted. New inbound flows are dropped.
    iptables -t nat -A POSTROUTING -o "$EXT_IF" -j MASQUERADE
    iptables -A FORWARD -i "$BR_IF" -j ACCEPT
    iptables -A FORWARD -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
    iptables -A FORWARD -j DROP
    ;;
  symmetric)
    # Per-destination source-port mapping approximation. True symmetric NAT
    # allocates a distinct public (ip,port) per destination; stock iptables
    # MASQUERADE reuses one source port per peer (endpoint-independent), so
    # this is only an approximation. We use MASQUERADE as the base and note
    # that authentic symmetric behavior needs a userspace NAT (see notes).
    iptables -t nat -A POSTROUTING -o "$EXT_IF" -j MASQUERADE
    iptables -A FORWARD -i "$BR_IF" -j ACCEPT
    iptables -A FORWARD -j ACCEPT
    ;;
  udpblocked)
    # Drop all UDP egress -> peers cannot punch; forced onto the relay.
    # Allow TCP so QUIC control/streams can still establish via the relay.
    iptables -t nat -A POSTROUTING -o "$EXT_IF" -j MASQUERADE
    iptables -A FORWARD -p udp -j DROP
    iptables -A FORWARD -j ACCEPT
    ;;
  *)
    echo "unknown nat type: $NAT_TYPE"; exit 1
    ;;
esac

echo "deployed $NAT_TYPE gateway with peers: ${PEERS[*]}"
