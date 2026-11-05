# txflow

Describe a Cardano transaction flow, see its UTxO graph, and run it on the
Leios Musashi test network.

<p align="center">
  <strong><a href="https://txflow.fun">txflow.fun</a></strong> · <a href="https://txflow.fun/spec.html">Spec reference</a>
</p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/editor-dark.png">
    <img alt="The txflow editor: a spec drawn as a UTxO graph, funded from one transaction, with two competing chains and a three-way double spend sent to relays in Frankfurt, Virginia and Tokyo" src="docs/images/editor-light.png">
  </picture>
</p>

An experiment is a short YAML spec: which transaction spends which output, where
transactions deliberately conflict, and which relay gets each one when. It has
no transaction ids, so the same spec reruns on the next node build or another
network.

## What is here

- **The spec**: its grammar, the pattern catalogue, expansion into a UTxO graph
  and the checks (`crates/txflow-core`), versioned as a JSON Schema
  (`schema/`).
- **The CLI** (`crates/txflow`): check a spec, expand it, and materialise it
  offline into signed Cardano transactions.
- **The browser build** of the check (`crates/txflow-wasm`).
- **The relay bridge** (`scripts/relay-bridge.mjs`, `deploy/`): a small HTTP
  server beside a node that submits signed transactions through `cardano-cli`
  and serves the node's own mempool and block events from its trace.
- **The network file** for the Leios Musashi test network (`networks/`) and
  example specs (`examples/`).

[txflow.fun](https://txflow.fun) is one site built on these: it turns plain
words into a spec, draws its graph, and runs it on Musashi.

## A spec

```yaml
version: 0
experiment: relay-race
seed: 1
wallets:
  alice: {}
patterns:
  - id: chains
    use: conflicting_chains        # two chains, every step contested
    with: { length: 3, conflict: per_step }
schedule:
  releases:
    - { select: { label: side-a }, target: frankfurt }
    - { select: { label: side-b }, target: tokyo }
```

Patterns (`chain`, `conflict_set`, `fan_out`, `branch_merge`, …) are the
building blocks. Repeat them with `count`, join them with your own
transactions, and time each release. Tag transactions with your own `metadata`,
or a `color` that `mempool-monitor` paints them in
([example](examples/tagged-double-spend.yaml)). Give transactions a script
`cost` such as `{ cpu: 25% }` of the network's per-transaction budget and each
one runs a script that uses that much
([example](examples/heavy-conflicting-pairs.yaml)). Give them a `validity`
window such as `{ until: 3, anchor: tip }`, in slots after the run's start or its
tip, and chain short-lived txs with a `step` per generation
([example](examples/expiring-chain.yaml)). Mint and burn native assets
under time-lock `policies`, each its own policy id, and move UTxOs that hold
hundreds of them: `asset_shuffle` mints them, merges the UTxOs into one and
splits it again with the assets shuffled
([example](examples/asset-shuffle.yaml)). Every field is in the
[spec reference](https://txflow.fun/spec.html); the JSON Schema is at
[txflow.fun/schema/v0.json](https://txflow.fun/schema/v0.json) (source in
[`schema/v0/`](schema/v0/)).

## The CLI

```sh
cargo run -p txflow -- check examples/relay-race.yaml --network networks/musashi.json
cargo run -p txflow -- expand examples/relay-race.yaml       # the transaction graph
cargo run -p txflow -- materialise examples/conflicting-chains.yaml \
  --context examples/demo-context.yaml -o manifest.json      # signed txs, offline
cargo run -p txflow -- schema                                # the JSON Schema
```

Builds for Linux and macOS are on the [releases page](../../releases), next to
each spec version's schema. The demo context uses a publicly known dummy key and
a fake funding UTxO, so its transactions are well formed but no node will
accept them.

### Larger runs

The caps in [`limits.json`](limits.json) (500 transactions, paths of 100, …)
are one deployment's policy, not part of the spec. The CLI holds a spec to a
limits file only when you pass `--limits`, so a spec with thousands of
transactions checks, expands and materialises on your machine (`txflow`
here is a release build, or `cargo run -p txflow --`):

```sh
txflow check big.yaml --network networks/musashi.json   # no caps
txflow check big.yaml --limits limits.json              # what that deployment would accept
txflow materialise big.yaml --context my-context.yaml -o manifest.json
```

`materialise` builds and signs; it sends nothing. For a real run, copy
[`examples/demo-context.yaml`](examples/demo-context.yaml) and offer UTxOs
held by your own payment key, then submit the manifest's `txs` in order
(funding first, each as `cbor_hex`) with `cardano-cli transaction submit`
against a node you can reach, or through a [relay bridge](docs/bridge.md)'s
`POST /submit`. The spec's `schedule` says which relay gets which
transaction and when; the manifest keeps each one's `name` and `labels` to
match them.

**With an LLM.** An LLM agent that can run commands in a checkout can write
and drive these specs. `txflow catalogue`, `txflow limits <file>` and
`txflow network <file>` print Markdown written for a person or a model
writing a spec, and `txflow schema` the JSON Schema. Have it write the YAML
and run `txflow check` until it passes, then `expand` to see the graph. Give
it the context file's path, never a signing key's contents.

## Repository

| Path | What |
|---|---|
| `crates/txflow-core` | The spec, pattern catalogue, expansion and checks. No network or crypto, so it also runs in the browser as WebAssembly. |
| `crates/txflow` | Offline materialise (Pallas) and the `txflow` CLI. |
| `crates/txflow-wasm` | Browser bindings: check a spec and get the JSON Schema. |
| `schema/`, `examples/`, `networks/`, `limits.json` | The generated schema, example specs, the Musashi relays and an example limits file. |
| `scripts/`, `deploy/`, `test/` | The relay bridge, its setup on a relay or a gateway, and its tests. |

- [Working on txflow](docs/development.md): the code, tests and changing the
  grammar.
- [The relay bridge](docs/bridge.md): running it on a relay or a gateway.
- Design: [spec](specs/spec.md), [data model](specs/data-model.md),
  [decisions](specs/research.md).

## License

[Apache 2.0](LICENSE)
