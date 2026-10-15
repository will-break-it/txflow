import { test } from "node:test";
import assert from "node:assert/strict";
import { chmod, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { TraceIndex, bridge, config, followTrace, limiter } from "../scripts/relay-bridge.mjs";

// A stand-in cardano-cli: logs its arguments, answers `query tip`, and
// rejects any submitted tx whose envelope is not Dijkstra.
async function fakeCli(dir) {
  const cli = join(dir, "cardano-cli");
  await writeFile(cli, `#!/bin/sh
echo "$CARDANO_NODE_SOCKET_PATH $*" >> "${dir}/calls"
case "$*" in
  "query tip"*) echo '{"era":"Dijkstra","slot":42}' ;;
  *"query utxo"*) echo '{"ab#0":{"value":{"lovelace":5}}}' ;;
  *"tx-mempool info"*) echo '{"capacityInBytes":180224,"numberOfTxs":3,"sizeInBytes":1250,"slot":42}' ;;
  *"tx-exists aaaa"*) echo '{"exists":true,"slot":42,"txId":"aa"}' ;;
  *"tx-exists"*) echo '{"exists":false,"slot":42}' ;;
  *"transaction submit"*)
    file=$(echo "$*" | sed 's/.*--tx-file //')
    grep -q '"cborHex":"5e5e"' "$file" && sleep 5
    grep -q '"Tx DijkstraEra"' "$file" && echo "Transaction successfully submitted." && exit 0
    echo "bad era" >&2; exit 1 ;;
esac
`);
  await chmod(cli, 0o755);
  return cli;
}

async function start(env = {}, trace = null) {
  const dir = await mkdtemp(join(tmpdir(), "bridge-test-"));
  for (const id of ["frankfurt", "virginia"]) await writeFile(join(dir, `musashi-${id}.socket`), "");
  const cfg = config({ CARDANO_CLI: await fakeCli(dir), SOCKET_DIR: dir, ...env });
  const server = bridge(cfg, trace).listen(0, "127.0.0.1");
  await new Promise((r) => server.once("listening", r));
  const base = `http://127.0.0.1:${server.address().port}`;
  return { dir, base, close: () => server.close() };
}

test("bridge reports relays, tips and UTxOs through the relay's socket", async () => {
  const b = await start();
  try {
    const relays = await (await fetch(`${b.base}/relays`)).json();
    assert.deepEqual(relays.map((r) => [r.id, r.open]), [
      ["frankfurt", true], ["virginia", true], ["tokyo", false],
      ["oregon", false], ["sydney", false], ["saopaulo", false], ["mumbai", false],
    ]);

    const tip = await (await fetch(`${b.base}/tip?relay=frankfurt`)).json();
    assert.equal(tip.slot, 42);

    const utxo = await fetch(`${b.base}/utxo?relay=virginia&address=addr_test1qxyz`);
    assert.deepEqual(await utxo.json(), { "ab#0": { value: { lovelace: 5 } } });
    const calls = await readFile(join(b.dir, "calls"), "utf8");
    assert.match(calls, /musashi-virginia\.socket dijkstra query utxo --address addr_test1qxyz --testnet-magic 164/);

    const pool = await (await fetch(`${b.base}/mempool?relay=frankfurt&tx=${"aa".repeat(32)}&tx=${"bb".repeat(32)}`)).json();
    assert.deepEqual(pool, { relay: "frankfurt", txs: { ["aa".repeat(32)]: true, ["bb".repeat(32)]: false } });
    assert.match(await readFile(join(b.dir, "calls"), "utf8"), /dijkstra query tx-mempool tx-exists bbbb[0-9a-f]* --testnet-magic 164/);
    assert.equal((await fetch(`${b.base}/mempool?relay=frankfurt&tx=nothex`)).status, 400);

    assert.equal((await fetch(`${b.base}/tip?relay=tokyo`)).status, 503, "no forward open");
    assert.equal((await fetch(`${b.base}/tip?relay=mars`)).status, 400);
    assert.equal((await fetch(`${b.base}/utxo?relay=frankfurt&address=;rm`)).status, 400);
    await fetch(`${b.base}/utxo?relay=frankfurt&address=addr_test1qa&address=addr_test1qb`);
    assert.match(await readFile(join(b.dir, "calls"), "utf8"), /--address addr_test1qa --address addr_test1qb/);
  } finally {
    b.close();
  }
});

test("bridge submits signed CBOR in the relay's era", async () => {
  const b = await start();
  try {
    const send = (body) => fetch(`${b.base}/submit?relay=frankfurt`, { method: "POST", body });
    const ok = await (await send(JSON.stringify({ cbor_hex: "83a0a0f6" }))).json();
    assert.equal(ok.accepted, true);
    assert.equal(typeof ok.reply_ms, "number");
    assert.equal((await send(JSON.stringify({ cbor_hex: "zz" }))).status, 400);
    assert.equal((await send("not json")).status, 400);
  } finally {
    b.close();
  }
});

test("bridge requires the bearer token when one is set", async () => {
  const b = await start({ BRIDGE_TOKEN: "t0k" });
  try {
    assert.equal((await fetch(`${b.base}/relays`)).status, 401);
    const ok = await fetch(`${b.base}/relays`, { headers: { authorization: "Bearer t0k" } });
    assert.equal(ok.status, 200);
  } finally {
    b.close();
  }
});

test("on a relay host the bridge serves only that relay, and only with a long token", async () => {
  assert.throws(() => config({ RELAY: "frankfurt", SOCKET: "/run/musashi/node.socket" }), /BRIDGE_TOKEN/);
  assert.throws(() => config({ RELAY: "mars", SOCKET: "/s", BRIDGE_TOKEN: "x".repeat(32) }), /not in networks/);
  const token = "k".repeat(40);
  const dir = await mkdtemp(join(tmpdir(), "bridge-host-"));
  const socket = join(dir, "node.socket");
  await writeFile(socket, "");
  const b = await start({ RELAY: "tokyo", SOCKET: socket, BRIDGE_TOKEN: token });
  try {
    const auth = { headers: { authorization: `Bearer ${token}` } };
    assert.deepEqual(await (await fetch(`${b.base}/relays`, auth)).json(), [{ id: "tokyo", socket, open: true }]);
    assert.equal((await fetch(`${b.base}/tip?relay=tokyo`, auth)).status, 200);
    assert.equal((await fetch(`${b.base}/tip?relay=frankfurt`, auth)).status, 400);
    assert.equal((await fetch(`${b.base}/tip?relay=tokyo`, { headers: { authorization: `Bearer ${token.slice(1)}` } })).status, 401);
  } finally {
    b.close();
  }
});

// Lines in the node's machine-trace format (from the Musashi relays,
// shortened): two txs added, a block carrying one of them adopted, both
// removed (one landed, one lost its input to a rival), one rejected, and an
// endorser block referencing the other by its serialized hash.
const A = "a1".repeat(32);
const B = "b2".repeat(32);
const C = "c3".repeat(32);
const txOf = (id, hash) =>
  `{"txBodyOmitted":true,"txIdFull":"${id}","txIdentity":{"bytes":285,"ledgerTxId":"${id}","serializedTxHash":"${hash}","status":"mapped"},"txid":"${id.slice(0, 8)}"}`;
const TRACE = [
  `{"at":"2026-10-14T07:19:58.733062256Z","ns":"Mempool.AttemptAdd","data":{"kind":"TraceMempoolAttemptingAdd","tx":${txOf(A, "5a".repeat(32))}},"sev":"Debug","thread":"1","host":"relay"}`,
  `{"at":"2026-10-14T07:19:58.733670478Z","ns":"Mempool.AddedTx","data":{"kind":"TraceMempoolAddedTx","mempoolSize":{"bytes":289,"numTxs":1},"tx":${txOf(A, "5a".repeat(32))}},"sev":"Info","thread":"1","host":"relay"}`,
  `{"at":"2026-10-14T07:20:01.698858667Z","ns":"Mempool.AddedTx","data":{"kind":"TraceMempoolAddedTx","mempoolSize":{"bytes":578,"numTxs":2},"tx":${txOf(B, "5b".repeat(32))}},"sev":"Info","thread":"1","host":"relay"}`,
  `{"at":"2026-10-14T07:20:02.000000000Z","ns":"LeiosFetch.Remote.Receive.Block","data":{"kind":"Recv","msg":{"ebHash":"${"e0".repeat(32)}","kind":"MsgLeiosBlock","txReferences":[{"bytes":285,"index":0,"serializedTxHash":"${"5b".repeat(32)}"},{"bytes":247,"index":1,"serializedTxHash":"${"99".repeat(32)}"}]}},"sev":"Debug","thread":"1","host":"relay"}`,
  `{"at":"2026-10-14T07:20:17.181907287Z","ns":"BlockFetch.Remote.Receive.Block","data":{"kind":"Recv","msg":{"blockHash":"${"bb".repeat(32)}","blockSize":1037,"kind":"MsgBlock","txIds":["${A}"]}},"sev":"Debug","thread":"1","host":"relay"}`,
  `{"at":"2026-10-14T07:20:17.19916735Z","ns":"ChainDB.AddBlockEvent.AddedToCurrentChain","data":{"headers":[{"blockNo":"145041","hash":"\\"${"bb".repeat(32)}\\"","kind":"ShelleyBlock","slotNo":"3224969"}],"kind":"AddedToCurrentChain"},"sev":"Notice","thread":"1","host":"relay"}`,
  `{"at":"2026-10-14T07:20:17.356932034Z","ns":"Mempool.RemoveTxs","data":{"kind":"TraceMempoolRemoveTxs","mempoolSize":{"bytes":0,"numTxs":0},"txs":[{"err":{"kind":"ApplyTxError","reason":["LedgerFailure (DijkstraUtxowFailure (UtxoFailure (BadInputsUTxO (NonEmptySet (fromList [TxIn (TxId {unTxId = SafeHash \\"0fea\\"}) (TxIx {unTxIx = 0})])))))","LedgerFailure (DijkstraUtxowFailure (UtxoFailure (ValueNotConservedUTxO Mismatch)))"]},"tx":${txOf(B, "5b".repeat(32))}},{"err":{"kind":"ApplyTxError","reason":["AllInputsAreSpent"]},"tx":${txOf(A, "5a".repeat(32))}}]},"sev":"Info","thread":"1","host":"relay"}`,
  `{"at":"2026-10-14T07:20:20.5Z","ns":"Mempool.RejectedTx","data":{"err":{"kind":"ApplyTxError","reason":["AllInputsAreSpent"]},"errdetails":"MempoolRejectedByLedger","kind":"TraceMempoolRejectedTx","tx":${txOf(C, "5c".repeat(32))}},"sev":"Info","thread":"1","host":"relay"}`,
];

test("the trace index keeps a tx's mempool and block events at the node's own times", () => {
  const index = new TraceIndex({ start: 0 });
  for (const line of TRACE) index.ingest(line);
  index.ingest("not json");
  index.ingest(`{"at":"2026-10-14T07:20:21Z","ns":"LeiosNotify.Remote.Receive.Votes","data":{}}`);

  const a = index.events(null, [A]).events.map((e) => [e.kind, e.at, e.reason ?? e.block?.number ?? null]);
  assert.deepEqual(a, [
    ["added", "2026-10-14T07:19:58.733670478Z", null],
    ["block", "2026-10-14T07:20:17.19916735Z", 145041],
    ["removed", "2026-10-14T07:20:17.356932034Z", "AllInputsAreSpent"],
  ]);
  const b = index.events(null, [B]).events;
  assert.deepEqual(b.map((e) => [e.kind, e.reason ?? e.eb ?? null]), [
    ["added", null],
    ["eb", "e0".repeat(32)],
    ["removed", "BadInputsUTxO"],
  ]);
  assert.equal(b[0].at_ms, Date.parse("2026-10-14T07:20:01.698Z") + 0.859);
  assert.deepEqual(index.events(null, [C]).events.map((e) => [e.kind, e.reason]), [["rejected", "AllInputsAreSpent"]]);
  assert.equal(index.tip.number, 145041);

  // A cursor gives only what is new.
  const all = index.events(null);
  assert.equal(all.events.length, 7);
  assert.deepEqual(index.events(all.cursor).events, []);
  const late = index.events(all.cursor - 1, [A, B, C]).events;
  assert.deepEqual(late.map((e) => e.kind), ["rejected"]);
});

test("the trace index is bounded and says when a reader fell behind", () => {
  const index = new TraceIndex({ maxEvents: 3, start: 0 });
  for (const line of TRACE) index.ingest(line);
  const { events, gap, oldest } = index.events(1);
  assert.equal(events.length, 3);
  assert.equal(gap, true);
  assert.equal(oldest, 5);
  assert.deepEqual(index.events(null, [B]).events.map((e) => e.kind), ["removed"]);
});

test("a tx a reader asked about keeps its events past the bound, for an hour", () => {
  let now = 0;
  const index = new TraceIndex({ maxEvents: 3, start: 0, now: () => now });
  for (const line of TRACE.slice(0, 2)) index.ingest(line); // A added
  const { cursor } = index.events(null, [A]); // a run starts watching A
  for (const line of TRACE.slice(2)) index.ingest(line); // busy node: the log keeps only the newest 3
  assert.equal(index.events(cursor).gap, true);
  assert.deepEqual(index.events(null, [A]).events.map((e) => e.kind), ["added", "block", "removed"]);
  assert.deepEqual(index.events(cursor, [A]).events.map((e) => e.kind), ["block", "removed"]);

  // Unwatched after an hour without asking: then only the bound counts.
  now += 60 * 60_000 + 1;
  index.watch([]);
  assert.equal(index.watched.size, 0);
  assert.equal(index.kept.size, 0);
});

test("a fork switch moves the tip and carries the new blocks' txs", () => {
  const index = new TraceIndex({ start: 0 });
  const line = (ns, data) => JSON.stringify({ at: "2026-10-14T08:00:00.5Z", ns, data });
  index.ingest(line("BlockFetch.Remote.Receive.Block", { msg: { blockHash: "b".repeat(64), txIds: [A] } }));
  index.ingest(line("ChainDB.AddBlockEvent.AddedToCurrentChain", { headers: [{ blockNo: "100", hash: `"${"a".repeat(64)}"`, slotNo: "5" }] }));
  index.ingest(
    line("ChainDB.AddBlockEvent.SwitchedToAFork", {
      headers: [
        { blockNo: "100", hash: `"${"b".repeat(64)}"`, slotNo: "6" },
        { blockNo: "101", hash: `"${"c".repeat(64)}"`, slotNo: "7" },
      ],
    }),
  );
  assert.deepEqual([index.tip.number, index.tip.hash], [101, "c".repeat(64)]);
  assert.deepEqual(index.events(null, [A]).events.map((e) => [e.kind, e.block.number]), [["block", 100]]);
});

test("a restarted bridge's cursors are above its old ones, so a reader gets everything again", () => {
  const before = new TraceIndex({ start: 1_000 });
  for (const line of TRACE) before.ingest(line);
  const { cursor } = before.events(null, [A]);
  const after = new TraceIndex(); // the bridge restarted and read the current file again
  for (const line of TRACE) after.ingest(line);
  assert.ok(after.events(null).cursor > cursor);
  assert.deepEqual(after.events(cursor, [A]).events.map((e) => e.kind), ["added", "block", "removed"]);
  assert.ok(Number.isSafeInteger(after.events(null).cursor));
});

test("the bridge follows the node's trace across a rotation and serves /events", async () => {
  const dir = await mkdtemp(join(tmpdir(), "bridge-trace-"));
  const first = join(dir, "node-1.json");
  const second = join(dir, "node-2.json");
  const current = join(dir, "node.json");
  await writeFile(first, `${TRACE.slice(0, 2).join("\n")}\n${TRACE[2].slice(0, 40)}`); // the last line half written
  await symlink(first, current);
  const index = new TraceIndex();
  const stop = followTrace(current, index, { everyMs: 10 });
  const b = await start({ RELAY: "frankfurt", SOCKET: join(dir, "socket"), BRIDGE_TOKEN: "t".repeat(32) }, index);
  const ask = (path, init = {}) =>
    fetch(`${b.base}${path}`, { ...init, headers: { authorization: `Bearer ${"t".repeat(32)}`, ...init.headers } });
  try {
    await waitFor(() => index.events(null).events.length === 1);
    await writeFile(first, `${TRACE.slice(0, 3).join("\n")}\n`); // the rest of that line arrives
    await waitFor(() => index.events(null).events.length === 2);
    await writeFile(second, `${TRACE.slice(3).join("\n")}\n`);
    await rm(current);
    await symlink(second, current); // the node rotates
    await waitFor(() => index.events(null).events.length === 7);

    const got = await (await ask(`/events?relay=frankfurt&tx=${A}`)).json();
    assert.equal(got.relay, "frankfurt");
    assert.equal(got.source, "trace");
    assert.equal(got.reading, true);
    assert.deepEqual(got.events.map((e) => e.kind), ["added", "block", "removed"]);
    const posted = await (
      await ask(`/events?relay=frankfurt`, { method: "POST", body: JSON.stringify({ since: got.cursor - 3, tx_ids: [A, B] }) })
    ).json();
    assert.deepEqual(posted.events.map((e) => [e.kind, e.tx]), [["removed", B], ["removed", A]]);
    assert.equal((await ask(`/events?relay=frankfurt&tx=nothex`)).status, 400);
    assert.equal((await ask(`/events?relay=frankfurt&since=x`)).status, 400);
    assert.equal((await ask(`/events?relay=tokyo`)).status, 400);
    assert.equal((await ask(`/events?relay=frankfurt`, { method: "POST", body: "null" })).status, 400);
  } finally {
    stop();
    b.close();
  }
});

test("a bridge whose node trace cannot be read says it is not reading it", async () => {
  const dir = await mkdtemp(join(tmpdir(), "bridge-trace-"));
  const index = new TraceIndex();
  const stop = followTrace(join(dir, "missing.json"), index, { everyMs: 10 });
  try {
    await new Promise((resolve) => setTimeout(resolve, 50));
    assert.equal(index.events(null).reading, false);
  } finally {
    stop();
  }
});

test("/node says how full the mempool is, and the tx cache and fetch bytes from the trace", async () => {
  const index = new TraceIndex({ start: 0 });
  const line = (at, ns, data) => JSON.stringify({ at, ns, data, sev: "Info", host: "relay" });
  index.ingest(line("2026-10-14T07:19:58.5Z", "Mempool.AddedTx", { kind: "TraceMempoolAddedTx", tx: { txIdFull: "dd".repeat(32) }, mempoolSize: { numTxs: 7, bytes: 2900 } }));
  index.ingest(line("2026-10-14T07:20:00Z", "LedgerMetrics", { slot: 2580300, utxoSize: 181944, chainDensity: 0.046 }));
  index.ingest(line("2026-10-14T07:20:28.9Z", "Consensus.LeiosKernel.BodyHits", {
    acquired: 0, cacheLoad: 0.027, cacheTxCount: 113163, ebHash: "13".repeat(32), ebSlot: 2580328,
    mempoolHits: 2, missedBoth: 375, tracked: 0, txsInEb: 377, validated: 0,
  }));
  index.ingest(line("2026-10-14T07:20:29.4Z", "Consensus.LeiosKernel.FetchTxsArrival", { evictedBytes: 0, extraBytes: 10, goodBytes: 93119, invalidBytes: 0 }));

  const b = await start({}, index);
  try {
    const node = await (await fetch(`${b.base}/node?relay=frankfurt`)).json();
    assert.equal(node.relay, "frankfurt");
    assert.deepEqual(node.mempool, { capacity_bytes: 180224, size_bytes: 1250, txs: 3, slot: 42 });
    assert.match(await readFile(join(b.dir, "calls"), "utf8"), /musashi-frankfurt\.socket dijkstra query tx-mempool info --testnet-magic 164/);
    assert.deepEqual(node.trace.mempool, { at: "2026-10-14T07:19:58.5Z", at_ms: Date.parse("2026-10-14T07:19:58.5Z"), txs: 7, bytes: 2900 });
    assert.equal(node.trace.ledger.utxo_size, 181944);
    const [hits] = node.trace.body_hits;
    assert.equal(hits.cache_tx_count, 113163);
    assert.equal(hits.missed_both, 375);
    assert.equal(hits.txs_in_eb, 377);
    assert.equal(node.trace.fetch_arrivals[0].good_bytes, 93119);
    // Only what happened since a time, when asked.
    const later = await (await fetch(`${b.base}/node?relay=frankfurt&since_ms=${Date.parse("2026-10-14T07:20:29Z")}`)).json();
    assert.deepEqual([later.trace.body_hits.length, later.trace.fetch_arrivals.length], [0, 1]);
    assert.equal((await fetch(`${b.base}/node?relay=frankfurt&since_ms=soon`)).status, 400);
  } finally {
    b.close();
  }
  // Without the node's trace, /node still says how full the mempool is.
  const plain = await start();
  try {
    const node = await (await fetch(`${plain.base}/node?relay=virginia`)).json();
    assert.equal(node.mempool.txs, 3);
    assert.equal(node.trace, null);
  } finally {
    plain.close();
  }
});

test("a bridge without the node's trace says so, so the Worker polls instead", async () => {
  const b = await start();
  try {
    const response = await fetch(`${b.base}/events?relay=frankfurt`);
    assert.equal(response.status, 404);
  } finally {
    b.close();
  }
});

async function waitFor(check, ms = 3_000) {
  const until = Date.now() + ms;
  while (!check()) {
    if (Date.now() > until) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 10));
  }
}

test("the index stays correct while it drops its oldest events, again and again", () => {
  const index = new TraceIndex({ maxEvents: 4, start: 0 });
  const id = (i) => i.toString(16).padStart(64, "0");
  const added = (i) =>
    `{"at":"2026-10-14T07:00:00.${String(i).padStart(9, "0")}Z","ns":"Mempool.AddedTx","data":{"tx":{"txIdFull":"${id(i)}"}}}`;
  for (let i = 1; i <= 30; i++) index.ingest(added(i));
  const all = index.events(null);
  assert.deepEqual(all.events.map((e) => e.seq), [27, 28, 29, 30], "only the newest four");
  assert.equal(all.oldest, 27);
  assert.ok(index.log.length - index.head === 4 && index.log.length <= 8, "the dropped front is let go");
  assert.deepEqual(index.events(28).events.map((e) => e.seq), [29, 30]);
  assert.equal(index.events(3).gap, true);
  assert.deepEqual(index.events(null, [id(29), id(2)]).events.map((e) => e.tx), [id(29)]);
});

test("a submit the node does not answer in time is a timeout, not a rejection", async () => {
  const b = await start({ CLI_TIMEOUT_MS: "300" });
  try {
    const send = (cbor_hex) => fetch(`${b.base}/submit?relay=frankfurt`, { method: "POST", body: JSON.stringify({ cbor_hex }) });
    const slow = await (await send("5e5e")).json();
    assert.equal(slow.accepted, false);
    assert.equal(slow.timeout, true);
    assert.match(slow.error, /did not answer within 0.3 s/);
    const refused = await (await send("83a0a0f6")).json();
    assert.equal(refused.timeout, undefined, "a quick answer is not a timeout");
  } finally {
    b.close();
  }
});

test("at most the limit of calls run at once; the rest wait their turn", async () => {
  const limit = limiter(2);
  let running = 0;
  let most = 0;
  const order = [];
  const call = (i) => limit(async () => {
    running++;
    most = Math.max(most, running);
    await new Promise((r) => setTimeout(r, 20));
    running--;
    order.push(i);
    return i;
  });
  assert.deepEqual(await Promise.all([0, 1, 2, 3, 4].map(call)), [0, 1, 2, 3, 4]);
  assert.equal(most, 2);
  assert.deepEqual(order, [0, 1, 2, 3, 4]);
  await assert.rejects(limit(async () => { throw new Error("no"); }), /no/);
  assert.equal(await limit(async () => "still works"), "still works", "a failed call frees its place");
});

test("a submit the node holds does not take a query's place", async () => {
  const b = await start({ QUERY_LIMIT: "1", CLI_TIMEOUT_MS: "2000" });
  try {
    await fetch(`${b.base}/tip?relay=frankfurt`);
    const held = fetch(`${b.base}/submit?relay=frankfurt`, { method: "POST", body: JSON.stringify({ cbor_hex: "5e5e" }) });
    const started = performance.now();
    assert.equal((await (await fetch(`${b.base}/tip?relay=frankfurt`)).json()).slot, 42);
    assert.ok(performance.now() - started < 1000, "the query did not wait for the submit");
    assert.equal((await (await held).json()).timeout, true);
  } finally {
    b.close();
  }
});
