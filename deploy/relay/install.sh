#!/bin/sh
# Install the txflow relay bridge on one relay host, next to its node.
#
#   sudo RELAY=frankfurt TUNNEL_TOKEN_FILE=tunnel-token sh install.sh < bridge-token
#
# Reads the bridge token (at least 32 characters) on stdin, so it never shows
# up in a process list or shell history. Copies the bridge to /opt/txflow-bridge,
# keeps the token in /etc/txflow-bridge.env (root only), and runs the bridge as
# a systemd service on 127.0.0.1:8164 that can read the node's socket. With
# TUNNEL_TOKEN_FILE set, cloudflared runs that Cloudflare Tunnel, which is how
# the Worker reaches the bridge (docs/bridge.md); it only dials out, so no
# port opens. Run it again to update the bridge.
#
# Environment: RELAY (required: the relay's id in networks/musashi.json),
# SOCKET (/run/musashi/node.socket), CARDANO_CLI (found on PATH), NODE (found
# on PATH), NETWORK (musashi), TUNNEL_TOKEN_FILE (the tunnel's token; needs
# cloudflared on PATH, or CLOUDFLARED), TRACE_FILE (the node's current machine
# trace, readable by the socket's group; then /events serves the node's own
# mempool and block events).
set -eu

: "${RELAY:?set RELAY to this relay's id, e.g. frankfurt}"
SOCKET="${SOCKET:-/run/musashi/node.socket}"
NETWORK="${NETWORK:-musashi}"
NODE="${NODE:-$(command -v node || true)}"
CARDANO_CLI="${CARDANO_CLI:-$(command -v cardano-cli || true)}"
HERE="$(cd "$(dirname "$0")" && pwd)"

[ -n "$NODE" ] || { echo "node (Node.js 22 or newer) is not on PATH; set NODE" >&2; exit 1; }
[ -n "$CARDANO_CLI" ] || { echo "cardano-cli is not on PATH; set CARDANO_CLI" >&2; exit 1; }
[ -S "$SOCKET" ] || { echo "no node socket at $SOCKET; set SOCKET" >&2; exit 1; }

TOKEN="$(cat)"
[ "${#TOKEN}" -ge 32 ] || { echo "give the bridge token (32+ characters) on stdin" >&2; exit 1; }

install -d -m 755 /opt/txflow-bridge/scripts /opt/txflow-bridge/networks
install -m 644 "$HERE/../../scripts/relay-bridge.mjs" /opt/txflow-bridge/scripts/
install -m 644 "$HERE/../../networks/$NETWORK.json" /opt/txflow-bridge/networks/

umask 077
cat > /etc/txflow-bridge.env <<ENV
NETWORK=$NETWORK
RELAY=$RELAY
SOCKET=$SOCKET
CARDANO_CLI=$CARDANO_CLI
BRIDGE_PORT=8164
BRIDGE_TOKEN=$TOKEN
${TRACE_FILE:+TRACE_FILE=$TRACE_FILE}
ENV
umask 022

# The service runs as a throwaway user in the socket's group, so it can talk
# to the node and nothing else.
GROUP="$(stat -c %G "$SOCKET")"
sed -e "s|@NODE@|$NODE|" -e "s|@GROUP@|$GROUP|" "$HERE/txflow-bridge.service" > /etc/systemd/system/txflow-bridge.service
systemctl daemon-reload
systemctl enable --now txflow-bridge.service
systemctl restart txflow-bridge.service
sleep 1
systemctl --no-pager --lines=5 status txflow-bridge.service

if [ -n "${TUNNEL_TOKEN_FILE:-}" ]; then
  CLOUDFLARED="${CLOUDFLARED:-$(command -v cloudflared || true)}"
  [ -n "$CLOUDFLARED" ] || { echo "TUNNEL_TOKEN_FILE needs cloudflared on PATH; set CLOUDFLARED" >&2; exit 1; }
  TUNNEL_TOKEN="$(cat "$TUNNEL_TOKEN_FILE")"
  [ -n "$TUNNEL_TOKEN" ] || { echo "$TUNNEL_TOKEN_FILE is empty" >&2; exit 1; }
  install -d -m 700 /etc/txflow-bridge
  umask 077
  printf 'TUNNEL_TOKEN=%s\n' "$TUNNEL_TOKEN" > /etc/txflow-bridge/tunnel.env
  umask 022
  sed "s|@CLOUDFLARED@|$CLOUDFLARED|" "$HERE/txflow-tunnel.service" > /etc/systemd/system/txflow-tunnel.service
  systemctl daemon-reload
  systemctl enable --now txflow-tunnel.service
  systemctl restart txflow-tunnel.service
  sleep 1
  systemctl --no-pager --lines=5 status txflow-tunnel.service
fi
