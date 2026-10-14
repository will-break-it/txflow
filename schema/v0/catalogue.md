# Patterns (spec version 0)

Generated from `crates/txflow-core/src/catalogue.rs`; do not edit.

### `independent`

`n` unrelated transactions, each funded by its own source. A baseline load with no dependencies or conflicts.

Emits: sources s1..sN; txs t1..tN (t<i> spends s<i>).

- `n` (whole number 1..100000, default 1): Number of transactions.
- `outs` (whole number 1..1000, default 1): Outputs per transaction: one change output plus `outs - 1` minimum-value outputs.
- `lovelace` (lovelace, default 10 ADA, or more when `outs` needs it): Value of each source.
- `wallet` (wallet name): Wallet that owns everything the pattern creates. Required when the spec declares more than one wallet.

Example: `- { id: independent, use: independent, with: { n: 100, outs: 2 } }`

### `chain`

A chain t1 → t2 → … → tN where each transaction spends the previous one's change output. Tests how nodes handle dependent transactions.

Emits: source s; txs t1..tN (t1 spends s, t<i> spends t<i-1>.0).

- `length` (whole number 1..100000, default 2): Number of transactions in the chain.
- `outs` (whole number 1..1000, default 1): Outputs per transaction: one change output plus `outs - 1` minimum-value outputs.
- `lovelace` (lovelace, default 10 ADA, or enough for every step's fee when the chain is long): Value of the source that funds the chain.
- `wallet` (wallet name): Wallet that owns everything the pattern creates. Required when the spec declares more than one wallet.

Example: `- { id: chain, use: chain, with: { length: 10 } }`

### `fan_out`

One transaction `root` with `width` outputs, each spent by its own child transaction. One parent, many children.

Emits: source s; txs root, c1..cW (c<i> spends root.<i-1>).

- `width` (whole number 2..1000, default 2): Number of outputs of `root`, and of children.
- `lovelace` (lovelace, default 3 ADA per branch): Value of each branch; the last branch also gets the leftover.
- `wallet` (wallet name): Wallet that owns everything the pattern creates. Required when the spec declares more than one wallet.

Example: `- { id: fan_out, use: fan_out, with: { width: 5 } }`

### `fan_in`

`width` independent leaf transactions whose change outputs are all spent by one `merge` transaction. Many parents, one child.

Emits: sources s1..sW; txs l1..lW, merge (merge spends every l<i>.0).

- `width` (whole number 2..1000, default 2): Number of leaf transactions merged.
- `lovelace` (lovelace, default 3 ADA per leaf): Value of each leaf's source.
- `wallet` (wallet name): Wallet that owns everything the pattern creates. Required when the spec declares more than one wallet.

Example: `- { id: fan_in, use: fan_in, with: { width: 4 } }`

### `conflict_set`

`k` transactions that all spend the same source, so at most one of them can land: a k-way double spend.

Emits: source s; txs x1..xK (every x<i> spends s).

- `k` (whole number 2..100000, default 2): Number of competing transactions.
- `lovelace` (lovelace, default 10 ADA): Value of the contested source.
- `wallet` (wallet name): Wallet that owns everything the pattern creates. Required when the spec declares more than one wallet.

Example: `- { id: conflict_set, use: conflict_set, with: { k: 3 } }`

### `branch_merge`

`root` branches into `width` children, which one `merge` transaction joins again. Every branch shares `root` as an ancestor, so `merge` depends on all of them: a `fan_out` closed by a merge.

Emits: source s; txs root, c1..cW, merge (merge spends every c<i>.0).

- `width` (whole number 2..1000, default 2): Number of branches.
- `lovelace` (lovelace, default 3 ADA per branch): Value of each branch; the last branch also gets the leftover.
- `wallet` (wallet name): Wallet that owns everything the pattern creates. Required when the spec declares more than one wallet.

Example: `- { id: branch_merge, use: branch_merge, with: { width: 2 } }`

### `conflicting_chains`

Two chains a1 → … → aN and b1 → … → bN that compete. With `per_step` every pair aᵢ, bᵢ spends a shared input cᵢ, so each pair conflicts. With `root` only a1 and b1 conflict and the later transactions are orphaned by whichever side loses.

Emits: sources wa, wb (fund each side), c1..cN (shared inputs; only c1 with `root`); txs a1..aN, b1..bN, labelled side-a and side-b.

- `length` (whole number 1..100000, default 3): Length of each chain.
- `conflict` (one of `per_step`, `root`, default `per_step`): `per_step`: every pair conflicts. `root`: only the first pair conflicts.
- `lovelace` (lovelace, default 10 ADA, or enough for every step's fee when `root` chains are long): Value of each side's funding source.
- `conflict_lovelace` (lovelace, default 2 ADA): Value of each shared input cᵢ.
- `wallet` (wallet name): Wallet that owns everything the pattern creates. Required when the spec declares more than one wallet.

Example: `- { id: conflicting_chains, use: conflicting_chains, with: { length: 3, conflict: per_step } }`

