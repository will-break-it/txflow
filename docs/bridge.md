# The relay bridge

`scripts/relay-bridge.mjs` is how a client reaches a relay's node over HTTP:
it runs the relay's own `cardano-cli` against the node's socket and, given
the node's trace, serves the node's own mempool and block events. It never sees a key: it receives signed
transactions. It answers only with the bearer token, and listens on 127.0.0.1
only, so it is reached through a tunnel or a TLS proxy beside it.

```
client ──▶ Cloudflare Tunnel or TLS proxy ──▶ txflow-bridge on 127.0.0.1:8164 ── cardano-cli ── node socket
```

Endpoints: `GET /relays`, and with `?relay=<id>` (an id in
`networks/<network>.json`) `GET /tip`, `/pparams`, `/utxo`, `/mempool`,
`/node`, `/logs`, `POST /submit` and `GET|POST /events`.
To run one on your machine against forwarded sockets, see
[`development.md`](development.md#a-relay-bridge-on-your-machine).

The bridge runs in one of two places:

- **On each relay** (`deploy/relay/`): the bridge and cloudflared next to the
  node, one tunnel per relay. No new port, no extra machine, and a relay's IP can
  change without breaking anything. It changes the relay hosts, so it needs
  their owner's agreement and belongs in their configuration
  (`deploy/relay/txflow-bridge.nix` for NixOS, `install.sh` elsewhere).
- **On a gateway** (`deploy/gateway/`): one always-on Linux host forwards each
  relay's node socket over SSH and serves them all, through one tunnel. The relays need only a user for the gateway's key and 22 open
  to its address. The forwards follow the hosts in `networks/musashi.json`, so
  a relay that relaunches with a new IP needs that file updated and
  `setup.sh` run again, unless the relays get fixed addresses.

The bridge never sees a key: it receives signed transactions. It answers only
with the bearer token, and listens on 127.0.0.1 only.

## 1. Tokens

Make the bridge token once and keep it out of the repo:

```sh
openssl rand -hex 32 > bridge-token
```

For a Cloudflare Tunnel, create one tunnel per bridge in the Cloudflare
dashboard (one per relay, or one on a gateway) and copy each tunnel's token
into a file (`tunnel-token`). It goes only onto the host that runs that
tunnel.

## 2. Install it

On each relay with NixOS, import `deploy/relay/txflow-bridge.nix` (its header
has an example) with an environment file holding `BRIDGE_TOKEN=` and
`TUNNEL_TOKEN=`. On a relay without Nix, from a checkout of this repo (Node.js
20+, `cardano-cli` and `cloudflared` on the host):

```sh
sudo RELAY=frankfurt TUNNEL_TOKEN_FILE=tunnel-token sh deploy/relay/install.sh < bridge-token
curl -s -H "authorization: Bearer $(cat bridge-token)" "127.0.0.1:8164/tip?relay=frankfurt"
```

**Node-timestamped events.** Give a bridge on a relay host the node's machine
trace (`traceFile` in the Nix module, `TRACE_FILE=` for `install.sh`; on
Musashi, the `node.json` symlink in `/var/log/musashi/traces/<host>_3010/`),
and it follows that file. `GET /events` / `POST /events` then serve the
node's own mempool and block events, each at the node's timestamp, and a
client can use them instead of polling the node. A bridge
without a trace answers 404 there, and clients then poll, with approximate
times. One that has a trace but cannot read it (a
wrong path, no permission) answers `reading: false`, and clients poll too.

What the bridge remembers, all in memory (none of it on disk):

- **The newest 200,000 events** of all txs. Under Musashi's background load
  that is about 20 minutes on frankfurt and virginia and 5 on tokyo (9,000 to
  40,000 events a minute). Readers that fall further behind get `gap: true`.
- **The events of the txs a reader asked about**, for an hour after it last
  asked, past that bound. A client that asks every few seconds for its own txs keeps
  all their node times, however long the experiment, while the bridge runs.
  The cap is 20,000 txs, and a tx keeps at most 100 events beyond the bound.
- **After a restart, only the node's current trace file.** The node rotates
  it by size (every one to two minutes under load), and the archiver deletes
  the rotated files, so events from before that are gone. A client whose experiment spans
  a bridge restart falls back to polled times for the txs whose events it
  missed. Restart bridges between experiments, not during one.
- **The txs in the node's mempool**, from the same events: added on
  `Mempool.AddedTx`, gone on `Mempool.RemoveTxs`. When the node says it holds
  fewer txs than that (a removal the trace missed, or a restarted node), the
  oldest go first.

**Mempool sketches.** `GET /mempool?relay=<id>` without `tx=` answers from
that list: `txs` (as the trace tells it), `node_txs` (as the node last said),
`since_ms` (the earliest trace time read; a tx added before it is missing until
it leaves) and `bottom`, the `k` smallest tx ids (`&k=`, 2,048 by default, at
most 50,000), each cut to 16 hex characters. Tx ids are hashes, so the
smallest are a uniform sample, and two relays' sketches estimate how much
their mempools overlap, to about 1/sqrt(k), without either sending its whole
mempool. With `txs` up to `k` the sketch is the whole mempool. A bridge
without a trace answers 404.

On a gateway instead (Debian or Ubuntu, from a checkout; `CARDANO_CLI` is the
Linux build from the relays' Leios release):

```sh
sudo TUNNEL_TOKEN_FILE=tunnel-token sh deploy/gateway/setup.sh < bridge-token
```

It prints the gateway's public key. Each relay needs a `txflow` user with that
key, in the node socket's group, and 22 open to the gateway's address.

