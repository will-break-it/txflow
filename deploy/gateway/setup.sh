#!/bin/sh
# Set up a gateway: one always-on Linux host (Debian or Ubuntu with systemd)
# that runs the txflow bridge for every relay, reaching each relay's node
# socket over SSH, and a Cloudflare Tunnel for the Worker to reach the bridge.
# Nothing listens on a public address.
#
#   sudo TUNNEL_TOKEN_FILE=tunnel-token sh deploy/gateway/setup.sh < bridge-token
#
# The first run makes the gateway's SSH key and prints its public half: the
# relays need it for REMOTE_USER (in the node socket's group), and 22 open to
# this host's address. Run it again after the network file's hosts change
# (a relay relaunched) or to update the bridge; it keeps the key.
#
# Environment: CARDANO_CLI (Linux build from the relays' Leios release; found
# on PATH), NODE (Node.js 22+, found on PATH), CLOUDFLARED (found on PATH),
# NETWORK (musashi), REMOTE_USER (txflow), REMOTE_SOCKET
# (/run/musashi/node.socket), TUNNEL_TOKEN_FILE (the tunnel's token).
set -eu

NETWORK="${NETWORK:-musashi}"
REMOTE_USER="${REMOTE_USER:-txflow}"
REMOTE_SOCKET="${REMOTE_SOCKET:-/run/musashi/node.socket}"
NODE="${NODE:-$(command -v node || true)}"
CARDANO_CLI="${CARDANO_CLI:-$(command -v cardano-cli || true)}"
CLOUDFLARED="${CLOUDFLARED:-$(command -v cloudflared || true)}"
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$HERE/../.."
SOCKET_DIR=/run/txflow

[ -n "$NODE" ] || { echo "node (Node.js 22 or newer) is not on PATH; set NODE" >&2; exit 1; }
[ -n "$CARDANO_CLI" ] || { echo "cardano-cli is not on PATH; set CARDANO_CLI" >&2; exit 1; }
[ -n "$CLOUDFLARED" ] || { echo "cloudflared is not on PATH; set CLOUDFLARED" >&2; exit 1; }
: "${TUNNEL_TOKEN_FILE:?set TUNNEL_TOKEN_FILE to the tunnel token's file}"

TOKEN="$(cat)"
[ "${#TOKEN}" -ge 32 ] || { echo "give the bridge token (32+ characters) on stdin" >&2; exit 1; }
TUNNEL_TOKEN="$(cat "$TUNNEL_TOKEN_FILE")"
[ -n "$TUNNEL_TOKEN" ] || { echo "$TUNNEL_TOKEN_FILE is empty" >&2; exit 1; }

# The account the forwards and the bridge run as, and its SSH key.
id txflow >/dev/null 2>&1 || useradd --system --home-dir /var/lib/txflow --create-home --shell /usr/sbin/nologin txflow
install -d -m 700 -o txflow -g txflow /var/lib/txflow/.ssh
[ -f /var/lib/txflow/.ssh/id_ed25519 ] || runuser -u txflow -- ssh-keygen -q -t ed25519 -N "" -C "txflow-gateway" -f /var/lib/txflow/.ssh/id_ed25519
printf 'd %s 0750 txflow txflow -\n' "$SOCKET_DIR" > /etc/tmpfiles.d/txflow.conf
systemd-tmpfiles --create /etc/tmpfiles.d/txflow.conf

# The bridge and the network file it reads.
install -d -m 755 /opt/txflow/scripts /opt/txflow/networks
install -m 644 "$REPO/scripts/relay-bridge.mjs" /opt/txflow/scripts/
install -m 644 "$REPO/networks/$NETWORK.json" /opt/txflow/networks/

# One env file per relay, from the network file: the only place hosts live.
install -d -m 755 /etc/txflow /etc/txflow/relays
RELAYS="$("$NODE" -e '
  const net = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
  for (const r of net.relays) console.log(`${r.id} ${r.host}`);
' "$REPO/networks/$NETWORK.json")"
rm -f /etc/txflow/relays/*.env
echo "$RELAYS" | while read -r id host; do
  printf 'HOST=%s\nNETWORK=%s\nSOCKET_DIR=%s\nREMOTE_USER=%s\nREMOTE_SOCKET=%s\n' \
    "$host" "$NETWORK" "$SOCKET_DIR" "$REMOTE_USER" "$REMOTE_SOCKET" > "/etc/txflow/relays/$id.env"
done

umask 077
printf 'NETWORK=%s\nSOCKET_DIR=%s\nCARDANO_CLI=%s\nBRIDGE_PORT=8164\nBRIDGE_TOKEN=%s\n' \
  "$NETWORK" "$SOCKET_DIR" "$CARDANO_CLI" "$TOKEN" > /etc/txflow/bridge.env
chown txflow /etc/txflow/bridge.env
printf 'TUNNEL_TOKEN=%s\n' "$TUNNEL_TOKEN" > /etc/txflow/tunnel.env
umask 022

cp "$HERE/txflow-forward@.service" /etc/systemd/system/
sed "s|@NODE@|$NODE|" "$HERE/txflow-bridge.service" > /etc/systemd/system/txflow-bridge.service
sed -e "s|@CLOUDFLARED@|$CLOUDFLARED|" -e "s|/etc/txflow-bridge/tunnel.env|/etc/txflow/tunnel.env|" \
  "$HERE/../relay/txflow-tunnel.service" > /etc/systemd/system/txflow-tunnel.service
systemctl daemon-reload

# Forwards for the relays in the file, none for relays no longer in it.
for unit in $(systemctl list-units --all --plain --no-legend 'txflow-forward@*' | awk '{print $1}'); do
  id="${unit#txflow-forward@}"; id="${id%.service}"
  [ -f "/etc/txflow/relays/$id.env" ] || systemctl disable --now "$unit"
done
echo "$RELAYS" | while read -r id host; do
  systemctl enable "txflow-forward@$id.service"
  systemctl restart "txflow-forward@$id.service"
done
for unit in txflow-bridge txflow-tunnel; do
  systemctl enable "$unit.service"
  systemctl restart "$unit.service"
done
sleep 2
systemctl --no-pager --lines=3 status 'txflow-forward@*' txflow-bridge txflow-tunnel || true

echo
echo "The gateway's public key, for $REMOTE_USER on each relay:"
cat /var/lib/txflow/.ssh/id_ed25519.pub
