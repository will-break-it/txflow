# Working on txflow

How the code fits together, the tests, and how to change the spec grammar.
Running the relay bridge on a relay is in [`bridge.md`](bridge.md).

## How a spec becomes transactions

```
spec (YAML) ──▶ expand ──▶ graph IR ──▶ materialise ──▶ signed txs ──▶ submit
 schema/v0/     txflow-core              txflow (Pallas)               relay bridge
```

- **`txflow-core`** parses a spec, expands its patterns into a graph of
  transactions and checks it: references resolve, nothing is spent twice along
  one path, the schedule names known relays, and the size limits hold. It has
  no network and no crypto, so `txflow-wasm` runs the same check in a browser.
- **`txflow`** materialises the graph against real UTxOs into signed
  transactions (Pallas), offline, and is the CLI.
- **The relay bridge** submits each signed transaction to its relay's node and
  reports what the node did with it.

## Networks, schedules and limits

A spec's `schedule` says where and when each transaction is sent: each release
selects transactions (by label, name or pattern) and sends them to one relay, at
an offset from the start and optionally spaced out.

[`networks/musashi.json`](../networks/musashi.json) lists the relays of the
Leios Musashi test network, named after the cities of their AWS regions
(`frankfurt`, `virginia`, `tokyo`, `oregon`, `sydney`, `saopaulo`, `mumbai`; the earlier ids `europe`, `americas` and
`asia` still work, with a warning). Specs, `txflow check --network` and
the sites built on txflow read it, and a release naming any other relay is an
error. Relay addresses are public configuration, not secrets. They change when
an instance is stopped. `via` defaults to `n2c`, so the node answers accept or
reject.

A temporary relay carries `until`, a UTC time written `YYYY-MM-DDTHH:MM:SSZ`.
From then on a site leaves it out of the network it serves: the model's
prompt, the page's checks and new runs no longer see it. `txflow check`
itself ignores `until`; it compares nothing with a clock.

A limits file (example: [`limits.json`](../limits.json)) caps what one
deployment accepts: transactions
in total, the longest path of dependent transactions, outputs per transaction
and total funding. The same code checks them everywhere: in the browser
(`txflow-wasm`), `txflow check --limits`, and before a run spends anything. Limits are
policy, not grammar, so changing them never changes the spec version.

**Timing.** A run's mempool, drop and block times are the relay nodes' own
when their bridges read the nodes' traces, and polled otherwise (see
[the bridge](bridge.md)).

## A relay bridge on your machine

`scripts/relay-bridge.mjs` is a small HTTP server on 127.0.0.1 that runs
`cardano-cli` (from the same Leios release as the relays) against each relay's
node socket, forwarded over SSH. It receives signed transactions only, never a
key. On Musashi the node's socket and n2n port are open to allow-listed
addresses only.

```sh
# For each relay in networks/musashi.json, forward its node socket:
ssh -fN -o ExitOnForwardFailure=yes -L /tmp/musashi-frankfurt.socket:/run/musashi/node.socket <user>@<frankfurt host>
CARDANO_CLI=/path/to/cardano-cli npm run bridge     # http://127.0.0.1:8164
curl -s 'http://127.0.0.1:8164/tip?relay=frankfurt'
```

## Tests

```sh
cargo test --workspace   # core, materialise, and the committed schema is up to date
npm test                 # the relay bridge, with a stand-in cardano-cli
```

## Changing the grammar

Edit the catalogue in `crates/txflow-core/src/catalogue.rs` (and the pattern
code in `patterns.rs` for a new pattern), then run `UPDATE_SCHEMA=1 cargo test`.
When the change would make existing specs invalid or mean something else, bump
`VERSION` in `crates/txflow-core/src/spec.rs` first. See
[`schema/README.md`](../schema/README.md).
