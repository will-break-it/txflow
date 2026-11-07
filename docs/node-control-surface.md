# Node control surface for adversarial / experiment work — scoping

**Status:** scoping note, pending review. As-of 2026-11-07.

Companion to txflow's Musashi experiments: txflow drives specs (conflicting
chains, double spends, relay-targeted releases) *into* a network; this note
scopes the complementary *node-side* controls — what a node we operate could be
made to do (filter its mempool, choose whether to announce or diffuse, prefer
our own relays, frontrun an observed tx).

## Goal

Catalogue the node controls we want for adversarial and experiment work —
mempool flags, announce/diffusion toggles, N2N on/off, peer and tx/address
allow/deny, a frontrunning mode — and say, for each: where the hook point is,
what already exists, and the effort to finish.

## Motivation

We want to run experiments where a node deviates from honest behaviour on
demand: filter or withhold mempool transactions, choose whether to announce or
diffuse, cut N2N, prefer our own relays and shun others, and observe a tx on an
N2N connection *without* ingesting it so we can act on it first (frontrunning).
Today these choices are scattered across internal functions in the Haskell node
with no external switch. We need one control surface, tunable at runtime.

## Key finding

Most of these controls already exist as a seam — in the **Rust Piranha node**
(`net-node` in the private `leios-adversarial-tools` repo), not the Haskell
node. Piranha is built on the honest consensus crates in
[`leios-tools`](https://github.com/input-output-hk/leios-tools) and driven by
the behaviour-tree engine in
[`shared-rs/consensus/src/behaviour/`](https://github.com/input-output-hk/leios-tools/tree/main/shared-rs/consensus/src/behaviour).
That engine emits one per-slot `ControlSignal`
([`tree/control.rs`](https://github.com/input-output-hk/leios-tools/blob/main/shared-rs/consensus/src/behaviour/tree/control.rs))
grouped by actuator — `praos` / `leios` / `mempool` — and ~26 adversarial
leaves each write their slice. It compiles from a TOML behaviour tree at load
and is **retuned live, per leaf**, through the `set_param` / `ParamOverrides`
seam. That is the "fine-tune control via switches" the ask describes, already
wired into sim-rs and Piranha.

**Recommendation:** do this work on Piranha / the behaviour engine, not by
forking the Haskell node. The Haskell `cardano-node` / `ouroboros-consensus`
`leios-prototype` branch has no control plane — only a static config file
(restart to change), on-chain protocol-parameter proposals (epoch latency), and
read-only traces/metrics. Reserve Haskell-node changes for effects that can
*only* be measured on the real Musashi node.

## The transaction path and where each control attaches

| Stage | Hook | Controls |
|---|---|---|
| Inbound N2N (TxSubmission2, LeiosNotify 18, LeiosFetch 19) | `drop_inbound`, `OutboundControl::DropTo`, `on_tx_received` | N2N off / partition; **new** peer allow/deny; **new** frontrun gate |
| Mempool admission (`applyTx`) | `MempoolControl.tx_filter`, `tx_flood_rate` | checksum filter; flood; **new** tx/address allow/deny |
| Mempool (Leios tx index, 2x capacity) | `tx_withholding_filter` | delay/suppress propagation, own-only, per-protocol |
| Forge -> EB + announce (`partitionMempool`) | `withhold_announce`, `announce_*`, `announce_eb_size`, `eb_burst` | announce or not; dangle; equivocate; lie about size; burst |
| Serve + vote (LeiosFetch, LeiosVoting) | `offer_eb_size`, `net_flood`, vote policy, `suppress_cert` | lie on offer; fetch/net flood; abstain; suppress cert |

## Control catalogue

Effort: **Exists** = shipped · **Small** = wire-up on an existing field ·
**Medium** = new leaf + one `ControlSignal` field + actuator · **Large** = new
consensus seam / Haskell fork.

| Control | Status | Effort |
|---|---|---|
| Mempool filter switch (`tx_filter`) | checksum-threshold policy (t22) | Exists |
| Withhold / delay tx propagation (`tx_withholding_filter`) | slot delay or suppress, own-only, per-protocol | Exists |
| Flood own mempool (`tx_flood_rate`) | drives local generator | Exists |
| Announce or not (`withhold_announce`) | suppresses EB announcement | Exists |
| Dangling / equivocating / fake announce | four announce leaves | Exists |
| Lie about EB size (`announce_eb_size` / `offer_eb_size`) | linear rewrite | Exists |
| N2N off / partition (`drop_inbound`, `DropTo`) | reset inbound; mute a peer set | Exists |
| Blacklist a block (`blacklist_blocks`) | offset-from-tip into blacklist | Exists |
| Allow own relays / deny others | partial: `net_flood_pools` (fail-closed) + `DropTo`; no standing peer filter | Medium |
| Tx / address allow-deny | partial: `tx_filter` keys on checksum, not identity | Medium |
| Frontrun (see on N2N, don't mempool) | new leaf; seam = `on_tx_received` + `BehaviourOutcome::Replace` + `MempoolControl` | Medium |

## Runtime control

Piranha has two tiers already: a TOML behaviour tree compiled at load
(`BtConfig::compile`, validated against the leaf vocabulary), and live per-leaf
`set_param` via the `ParamOverrides` / `action_override(behaviour_id, field)`
seam. The engine side is ready. The transport that carries a live `set_param`
to a running node (HTTP/socket control plane vs reload-only) lives in the
private repo and needs confirming. If absent, a thin HTTP endpoint forwarding
`(behaviour_id, field, value)` into the existing seam is the whole job.

## Open questions / next steps

1. Confirm the live-control transport in `leios-adversarial-tools`.
2. Peer allow/deny: promote to a first-class ingress/egress filter keyed on
   `PeerId`/pool; allow-list own relays, fail-closed.
3. Pin the frontrun leaf's observe-but-don't-ingest contract.
4. Tx/address allow-deny: add address parsing in the admission filter.
5. Decide which controls need a Haskell-node counterpart for on-Musashi
   measurement.
6. Obtain read access to `leios-adversarial-tools` (named-attack `behaviours/*.toml`).
