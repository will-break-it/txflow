#!/usr/bin/env node
// Relay bridge: the Worker's way to a relay's node, over HTTP.
//
// Locally it serves every relay, running cardano-cli against their node
// sockets forwarded over SSH:
//
//   ssh -N -L /tmp/musashi-frankfurt.socket:/run/musashi/node.socket <user>@<host>
//
// One forward per relay in networks/<network>.json. Deployed, one bridge runs
// on each relay host and serves only that relay's own socket (RELAY and
// SOCKET set); a TLS proxy in front of it (deploy/relay) is the only
// way in, and it then refuses to start without BRIDGE_TOKEN. Either way it
// listens on 127.0.0.1 only and never sees a signing key: it receives signed
// CBOR.
//
// Routes (relay = an id from the network file):
//   GET  /relays                       relays and whether their socket exists
//   GET  /tip?relay=                   cardano-cli query tip
//   GET  /pparams?relay=               cardano-cli query protocol-parameters
//   GET  /utxo?relay=&address=…        cardano-cli query utxo, as JSON (1-20 addresses)
//   GET  /mempool?relay=&tx=…          which of these tx ids (1-50) are in the relay's mempool
//   GET  /mempool?relay=&k=            without tx ids: the mempool as the node's trace
//                                      tells it, as a sketch of its k smallest tx ids
//   POST /submit?relay=  {"cbor_hex"}  cardano-cli transaction submit
//   GET  /events?relay=&since=&tx=…    the node's own mempool and block events (TRACE_FILE)
//   POST /events?relay=  {"since", "tx_ids"}   the same, for up to 5,000 tx ids
//   GET  /node?relay=&since_ms=        how full the node's mempool is, and from its
//                                      trace its tx cache, EB fetch bytes and ledger size
//   GET  /logs?relay=&from_ms=&to_ms=&ns=&limit=   the node's own trace lines in a
//                                      time window: the notable namespaces only (LOGGED)
//
// /events reads the node's machine trace instead of asking the node: when the
// node added a tx to its mempool, removed it (and why) or rejected it, and
// which adopted block carried it, each at the node's own timestamp. Asking
// costs the node nothing, however many runs ask (see TraceIndex below).
//
// Environment: NETWORK (musashi), BRIDGE_PORT (8164), CARDANO_CLI
// (cardano-cli), SOCKET_DIR (/tmp), BRIDGE_TOKEN (bearer token; optional
// locally), and on a relay host RELAY (its id), SOCKET (its node socket) and
// optionally TRACE_FILE (the node's current machine trace, e.g. the node.json
// symlink in /var/log/musashi/traces/<host>_3010/). QUERY_LIMIT (4) caps the
// cardano-cli queries running at once; submits are never held back.

import { execFile } from "node:child_process";
import { createHash, timingSafeEqual } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { mkdtemp, open, realpath, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const MAX_BODY = 512 * 1024;
const CLI_TIMEOUT_MS = 30_000;
// Queries at once per bridge: many runs polling together would otherwise
// start a cardano-cli each, on a host that also runs the node.
const QUERY_LIMIT = 4;

export function config(env = process.env) {
  const network = env.NETWORK || "musashi";
  const file = fileURLToPath(new URL(`../networks/${network}.json`, import.meta.url));
  const net = JSON.parse(readFileSync(file, "utf8"));
  const socketDir = env.SOCKET_DIR || "/tmp";
  if (env.RELAY) {
    // On a relay host: that relay only, through its own socket.
    if (!net.relays.some((r) => r.id === env.RELAY)) throw new Error(`RELAY ${env.RELAY} is not in networks/${network}.json`);
    if (!env.SOCKET) throw new Error("RELAY needs SOCKET, the node's socket path");
    if (!env.BRIDGE_TOKEN || env.BRIDGE_TOKEN.length < 32) throw new Error("a deployed bridge needs BRIDGE_TOKEN of at least 32 characters");
  }
  return {
    magic: net.magic,
    cli: env.CARDANO_CLI || "cardano-cli",
    port: Number(env.BRIDGE_PORT || 8164),
    cliTimeoutMs: Number(env.CLI_TIMEOUT_MS || CLI_TIMEOUT_MS),
    queryLimit: Number(env.QUERY_LIMIT || QUERY_LIMIT),
    token: env.BRIDGE_TOKEN || "",
    // Only a bridge on a relay host reads that relay's node trace.
    traceFile: env.RELAY && env.TRACE_FILE ? env.TRACE_FILE : null,
    relays: env.RELAY
      ? { [env.RELAY]: env.SOCKET }
      : Object.fromEntries(net.relays.map((r) => [r.id, join(socketDir, `${network}-${r.id}.socket`)])),
  };
}

function run(cfg, socket, args) {
  return new Promise((resolve) => {
    execFile(
      cfg.cli,
      args,
      { env: { ...process.env, CARDANO_NODE_SOCKET_PATH: socket }, timeout: cfg.cliTimeoutMs ?? CLI_TIMEOUT_MS },
      (error, stdout, stderr) =>
        resolve({
          ok: !error,
          // Killed at CLI_TIMEOUT_MS: the node did not answer (a full mempool holds a submit).
          timedOut: Boolean(error?.killed),
          stdout: stdout.trim(),
          stderr: (stderr || error?.message || "").trim(),
        }),
    );
  });
}

// At most `limit` of the calls given to the returned function run at once;
// the rest wait their turn, first come first served.
export function limiter(limit) {
  let running = 0;
  const waiting = [];
  return async (call) => {
    if (running >= limit) await new Promise((go) => waiting.push(go));
    running++;
    try {
      return await call();
    } finally {
      running--;
      waiting.shift()?.();
    }
  };
}

// One queue per config, so tests with their own config don't share one.
const queues = new WeakMap();
function runQuery(cfg, socket, args) {
  if (!queues.has(cfg)) queues.set(cfg, limiter(cfg.queryLimit ?? QUERY_LIMIT));
  return queues.get(cfg)(() => run(cfg, socket, args));
}

// The era the relay is in decides the cardano-cli command group and the
// text envelope type. Asked once per relay.
const eras = new Map();
async function era(cfg, relay) {
  if (eras.has(relay)) return eras.get(relay);
  const tip = await runQuery(cfg, cfg.relays[relay], ["query", "tip", "--testnet-magic", String(cfg.magic)]);
  if (!tip.ok) throw new BridgeError(502, tip.stderr || "query tip failed");
  const name = JSON.parse(tip.stdout).era;
  eras.set(relay, name);
  return name;
}

class BridgeError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

function json(res, status, body) {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
}

async function readBody(req) {
  let size = 0;
  const chunks = [];
  for await (const chunk of req) {
    size += chunk.length;
    if (size > MAX_BODY) throw new BridgeError(413, "body too large");
    chunks.push(chunk);
  }
  return Buffer.concat(chunks).toString("utf8");
}

function relayOf(cfg, url) {
  const relay = url.searchParams.get("relay");
  if (!relay || !(relay in cfg.relays)) {
    throw new BridgeError(400, `relay must be one of: ${Object.keys(cfg.relays).join(", ")}`);
  }
  if (!existsSync(cfg.relays[relay])) {
    throw new BridgeError(503, `no socket at ${cfg.relays[relay]}; is the SSH forward open?`);
  }
  return relay;
}

async function query(cfg, relay, args) {
  const eraName = (await era(cfg, relay)).toLowerCase();
  const out = await runQuery(cfg, cfg.relays[relay], [
    eraName, "query", ...args, "--testnet-magic", String(cfg.magic), "--output-json",
  ]);
  if (!out.ok) throw new BridgeError(502, out.stderr);
  return JSON.parse(out.stdout);
}

/** `query tx-mempool info`: {capacity_bytes, size_bytes, txs, slot}. */
async function mempoolInfo(cfg, relay) {
  const eraName = (await era(cfg, relay)).toLowerCase();
  const r = await runQuery(cfg, cfg.relays[relay], [eraName, "query", "tx-mempool", "info", "--testnet-magic", String(cfg.magic)]);
  if (!r.ok) throw new BridgeError(502, r.stderr || "query tx-mempool info failed");
  const info = JSON.parse(r.stdout);
  return {
    capacity_bytes: info.capacityInBytes ?? null,
    size_bytes: info.sizeInBytes ?? null,
    txs: info.numberOfTxs ?? null,
    slot: info.slot ?? null,
  };
}

// `query tx-mempool tx-exists` answers for one tx at a time, so ask for each,
// a few at once.
async function mempool(cfg, relay, ids) {
  const eraName = (await era(cfg, relay)).toLowerCase();
  const out = {};
  for (let i = 0; i < ids.length; i += 8) {
    await Promise.all(
      ids.slice(i, i + 8).map(async (id) => {
        const r = await runQuery(cfg, cfg.relays[relay], [
          eraName, "query", "tx-mempool", "tx-exists", id, "--testnet-magic", String(cfg.magic),
        ]);
        if (!r.ok) throw new BridgeError(502, r.stderr || "query tx-mempool failed");
        let exists;
        try {
          exists = JSON.parse(r.stdout).exists;
        } catch {
          exists = /"?exists"?\s*:\s*true/.test(r.stdout);
        }
        out[id] = exists === true;
      }),
    );
  }
  return out;
}

async function submit(cfg, relay, body) {
  let cborHex;
  try {
    cborHex = JSON.parse(body).cbor_hex;
  } catch {
    throw new BridgeError(400, "body must be JSON with cbor_hex");
  }
  if (typeof cborHex !== "string" || !/^([0-9a-f]{2})+$/.test(cborHex)) {
    throw new BridgeError(400, "cbor_hex must be lowercase hex");
  }
  const eraName = await era(cfg, relay);
  const dir = await mkdtemp(join(tmpdir(), "txflow-bridge-"));
  try {
    const file = join(dir, "tx.signed");
    await writeFile(file, JSON.stringify({ type: `Tx ${eraName}Era`, description: "", cborHex }));
    const started = performance.now();
    const out = await run(cfg, cfg.relays[relay], [
      eraName.toLowerCase(), "transaction", "submit", "--testnet-magic", String(cfg.magic), "--tx-file", file,
    ]);
    const reply_ms = Math.round(performance.now() - started);
    if (out.ok) return { relay, accepted: true, reply_ms };
    if (out.timedOut) {
      return { relay, accepted: false, timeout: true, reply_ms, error: `the node did not answer within ${(cfg.cliTimeoutMs ?? CLI_TIMEOUT_MS) / 1000} s (its mempool may be full)` };
    }
    return { relay, accepted: false, reply_ms, error: out.stderr };
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

// Compare without leaking how much of a guess was right.
function sameText(a, b) {
  const digest = (text) => createHash("sha256").update(text).digest();
  return timingSafeEqual(digest(a), digest(b));
}

export function bridge(cfg = config(), trace = null) {
  return createServer(async (req, res) => {
    try {
      if (cfg.token && !sameText(req.headers.authorization ?? "", `Bearer ${cfg.token}`)) {
        throw new BridgeError(401, "missing or wrong bearer token");
      }
      const url = new URL(req.url, "http://bridge");
      const route = `${req.method} ${url.pathname}`;
      if (route === "GET /relays") {
        return json(res, 200, Object.entries(cfg.relays).map(([id, socket]) => ({
          id, socket, open: existsSync(socket),
        })));
      }
      if (route === "GET /tip") {
        const relay = relayOf(cfg, url);
        const tip = await runQuery(cfg, cfg.relays[relay], ["query", "tip", "--testnet-magic", String(cfg.magic)]);
        if (!tip.ok) throw new BridgeError(502, tip.stderr);
        return json(res, 200, { relay, ...JSON.parse(tip.stdout) });
      }
      if (route === "GET /pparams") {
        return json(res, 200, await query(cfg, relayOf(cfg, url), ["protocol-parameters"]));
      }
      if (route === "GET /utxo") {
        const addresses = url.searchParams.getAll("address");
        if (!addresses.length || addresses.length > 20 || !addresses.every((a) => /^addr(_test)?1[0-9a-z]+$/.test(a))) {
          throw new BridgeError(400, "give 1 to 20 bech32 addresses");
        }
        const args = addresses.flatMap((a) => ["--address", a]);
        return json(res, 200, await query(cfg, relayOf(cfg, url), ["utxo", ...args]));
      }
      if (route === "GET /mempool" && !url.searchParams.has("tx")) {
        const relay = relayOf(cfg, url);
        if (!trace) throw new BridgeError(404, "this bridge does not read its node's trace");
        const k = Number(url.searchParams.get("k") ?? SKETCH_K);
        if (!Number.isInteger(k) || k < 1 || k > MAX_SKETCH_K) throw new BridgeError(400, `k must be 1 to ${MAX_SKETCH_K}`);
        return json(res, 200, { relay, ...trace.sketch(k) });
      }
      if (route === "GET /mempool") {
        const ids = url.searchParams.getAll("tx");
        if (!ids.length || ids.length > 50 || !ids.every((id) => /^[0-9a-f]{64}$/.test(id))) {
          throw new BridgeError(400, "give 1 to 50 tx ids");
        }
        const relay = relayOf(cfg, url);
        return json(res, 200, { relay, txs: await mempool(cfg, relay, ids) });
      }
      if (route === "GET /node") {
        const relay = relayOf(cfg, url);
        const since = url.searchParams.get("since_ms");
        if (since != null && !/^\d+$/.test(since)) throw new BridgeError(400, "since_ms must be milliseconds since the epoch");
        const mempool = await mempoolInfo(cfg, relay).catch((error) => ({ error: error.message }));
        return json(res, 200, { relay, at_ms: Date.now(), mempool, trace: trace ? trace.node(since == null ? null : Number(since)) : null });
      }
      if (route === "GET /logs") {
        const relay = relayOf(cfg, url);
        if (!trace) throw new BridgeError(404, "this bridge does not read its node's trace");
        const ms = (name) => {
          const v = url.searchParams.get(name);
          if (v != null && !/^\d+$/.test(v)) throw new BridgeError(400, `${name} must be milliseconds since the epoch`);
          return v == null ? null : Number(v);
        };
        const limit = Number(url.searchParams.get("limit") ?? 200);
        if (!Number.isInteger(limit) || limit < 1 || limit > MAX_LOG_LINES) throw new BridgeError(400, `limit must be 1 to ${MAX_LOG_LINES}`);
        const prefixes = url.searchParams.getAll("ns").filter(Boolean);
        return json(res, 200, { relay, ...trace.logs({ fromMs: ms("from_ms"), toMs: ms("to_ms"), prefixes, limit }) });
      }
      if (route === "POST /submit") {
        const relay = relayOf(cfg, url);
        return json(res, 200, await submit(cfg, relay, await readBody(req)));
      }
      if (route === "GET /events" || route === "POST /events") {
        const relay = url.searchParams.get("relay");
        if (!relay || !(relay in cfg.relays)) throw new BridgeError(400, `relay must be one of: ${Object.keys(cfg.relays).join(", ")}`);
        if (!trace) throw new BridgeError(404, "this bridge does not read its node's trace");
        let since = url.searchParams.get("since");
        let ids = url.searchParams.getAll("tx");
        if (req.method === "POST") {
          let body;
          try {
            body = JSON.parse(await readBody(req));
          } catch {
            throw new BridgeError(400, "body must be JSON with tx_ids");
          }
          if (body === null || typeof body !== "object") throw new BridgeError(400, "body must be JSON with tx_ids");
          since = body.since ?? null;
          ids = body.tx_ids ?? [];
        }
        if (!Array.isArray(ids) || ids.length > MAX_EVENT_TXS || !ids.every((id) => TX_ID.test(id))) {
          throw new BridgeError(400, `give up to ${MAX_EVENT_TXS} tx ids`);
        }
        if (since != null && since !== "" && !/^\d+$/.test(String(since))) throw new BridgeError(400, "since must be a cursor");
        return json(res, 200, { relay, ...trace.events(since == null || since === "" ? null : Number(since), ids) });
      }
      throw new BridgeError(404, `no route ${route}`);
    } catch (error) {
      json(res, error.status || 500, { error: error.message });
    }
  });
}

const TX_ID = /^[0-9a-f]{64}$/;
const MAX_EVENT_TXS = 5_000;
const MAX_EVENTS_REPLY = 5_000;
// A tx re-added and dropped over and over keeps only its first this many past the log's bound.
const MAX_KEPT_PER_TX = 100;
// And all watched txs together keep at most this many (~0.7 KB each).
const MAX_KEPT = 100_000;
// The namespaces the index keeps; every other line is skipped unparsed.
const KEPT = [
  "Mempool.AddedTx",
  "Mempool.RemoveTxs",
  "Mempool.RejectedTx",
  "BlockFetch.Remote.Receive.Block",
  "ChainDB.AddBlockEvent.AddedToCurrentChain",
  "ChainDB.AddBlockEvent.SwitchedToAFork",
  "LeiosFetch.Remote.Receive.Block",
  "Consensus.LeiosKernel.BodyHits",
  "Consensus.LeiosKernel.FetchTxsArrival",
  "LedgerMetrics",
];
// The node's own summaries (/node) keep only their latest this many each.
const NODE_HISTORY = 100;
// The trace lines /logs serves: what a node says about its mempool, consensus,
// chain and peers, as written. Per-tx mempool lines are left out (they flood
// the window under load, and /events has them per tx), as are the mux, keep
// alive and other chatter that makes up most of the trace.
const LOGGED = [
  "Mempool.",
  "Consensus.",
  "Forge.",
  "LedgerMetrics",
  "ChainDB.AddBlockEvent.",
  "ChainDB.CopyToImmutableDBEvent.",
  "ChainSync.Client.RolledBack",
  "ChainSync.Client.FoundIntersection",
  "ChainSync.Client.Termination",
  "BlockFetch.Client.CompletedBlockFetch",
  "LeiosFetch.Remote.Receive.",
  "LeiosNotify.Remote.Receive.",
  "Net.ConnectionManager.Remote.ConnectError",
  "Net.Mux.Remote.ExceptionExit",
  "Net.InboundGovernor.Remote.MuxErrored",
  "Net.PeerSelection.Counters",
];
const NOT_LOGGED = ["Mempool.AddedTx", "Mempool.RemoveTxs", "Mempool.AttemptAdd"];
export const logged = (ns) => LOGGED.some((p) => ns.startsWith(p)) && !NOT_LOGGED.includes(ns);
// A logged line longer than this is cut (an EB's tx list can run to megabytes).
const MAX_LINE_CHARS = 2_000;
// /logs answers with at most this many lines; the next window starts after the last.
const MAX_LOG_LINES = 1_000;
// A mempool sketch: its k smallest tx ids, each cut to SKETCH_CHARS hex
// characters. Tx ids are hashes, so the smallest are a uniform sample, and two
// relays' sketches estimate how much their mempools overlap (to about 1/sqrt(k))
// without either sending its whole mempool.
const SKETCH_K = 2_048;
const MAX_SKETCH_K = 50_000;
const SKETCH_CHARS = 16;

/** RFC 3339 with nanoseconds -> milliseconds since the epoch, to the microsecond. */
export function traceMs(at) {
  const m = /^(.*T\d\d:\d\d:\d\d)(?:\.(\d+))?Z$/.exec(at ?? "");
  if (!m) return null;
  const whole = Date.parse(`${m[1]}Z`);
  const fraction = Number(`0.${(m[2] ?? "0").padEnd(9, "0").slice(0, 9)}`);
  return Math.round((whole + fraction * 1000) * 1000) / 1000;
}

/** The short reason a node gave for removing or rejecting a tx. */
export function reasonOf(err) {
  const text = (err?.reason ?? []).join(" ");
  if (/AllInputsAreSpent/.test(text)) return "AllInputsAreSpent";
  // The ledger's own rule (BadInputsUTxO, ValueNotConservedUTxO, ...), not its wrappers.
  return /\b[A-Z]\w*UTxOW?\b/.exec(text)?.[0] ?? (text.slice(0, 120) || "unknown");
}

/**
 * The node's mempool and block events from its machine trace, newest last,
 * bounded: when full, the oldest go. Each event has a cursor (seq), so a
 * reader asks only for what is new; and the events of each tx are indexed, so
 * a run asks only for its own txs.
 *
 * Under load the bound is minutes of history (200,000 events is ~5 minutes at
 * 40,000 events a minute). So the txs a reader asks about are watched for
 * `watchMs` after it last asked: their events outlive the bound, however busy
 * the node is.
 *
 * Events: {seq, at, at_ms, kind, tx, ...}
 *   added     the node took the tx into its mempool
 *   removed   it left the mempool; reason AllInputsAreSpent when a block it
 *             adopted spent its inputs (normally: it landed), else why
 *   rejected  the node would not take it; reason
 *   block     a block the node adopted carries it; block {number, hash, slot}
 *   eb        a Leios endorser block the node received references it; eb
 */
export class TraceIndex {
  // Cursors start at the clock (in microseconds), so a restarted bridge's
  // cursors are above any it gave out before: a reader holding an old one gets
  // everything again (re-read from the current file) rather than nothing.
  constructor({
    maxEvents = 200_000,
    maxBlocks = 500,
    maxHashes = 200_000,
    maxWatched = 20_000,
    maxLines = 20_000,
    watchMs = 60 * 60_000,
    start = Date.now() * 1000,
    now = () => Date.now(),
  } = {}) {
    this.max = { events: maxEvents, blocks: maxBlocks, hashes: maxHashes, watched: maxWatched, lines: maxLines };
    this.watchMs = watchMs;
    this.now = now;
    // Events, oldest first, from log[head] on: dropping the oldest moves head
    // instead of shifting the array, which costs its length every time.
    this.log = [];
    this.head = 0;
    this.byTx = new Map(); // tx id -> its events still in the log, oldest first
    this.watched = new Map(); // tx id -> until when, oldest first
    this.kept = new Map(); // watched tx id -> its events the log has dropped, oldest first
    this.keptCount = 0;
    // Whether the node's trace is open: a bridge that cannot read it says so,
    // and readers ask the node instead.
    this.reading = false;
    this.bodies = new Map(); // block hash -> its tx ids, from the block's arrival
    this.serialized = new Map(); // a tx's serialized hash (EBs use it) -> its id
    this.seq = start;
    this.tip = null;
    // The node's state as its trace tells it, for /node: the mempool's size
    // after its last change, each arriving EB's hits in the mempool and the
    // tx cache, the bytes of EB tx fetches, and the ledger's size.
    this.state = { mempool: null, bodyHits: [], arrivals: [], ledger: null };
    // The txs in the node's mempool as its trace tells it: tx id -> when it
    // was added, oldest first. A removal the trace missed would leave a tx
    // here for good, so when the node says it holds fewer txs than this, the
    // oldest go first.
    this.pool = new Map();
    this.firstMs = null; // the earliest trace time read, for what the pool can know
    // The LOGGED lines as written (cut to MAX_LINE_CHARS), oldest first, for /logs.
    this.lines = [];
    this.linesHead = 0;
  }

  /**
   * The logged lines from `fromMs` to `toMs` (either open), only of the
   * namespaces starting with one of `prefixes` when given, at most `limit`
   * from the start of the window: {lines: [{at_ms, ns, line}], counts: lines
   * per namespace in the whole window, more, next_from_ms, oldest_ms}.
   */
  logs({ fromMs = null, toMs = null, prefixes = [], limit = 200 } = {}) {
    const lines = [];
    const counts = {};
    let more = false;
    let next = null;
    for (let i = this.linesHead; i < this.lines.length; i++) {
      const l = this.lines[i];
      if (fromMs != null && l.at_ms < fromMs) continue;
      if (toMs != null && l.at_ms > toMs) break;
      if (prefixes.length && !prefixes.some((p) => l.ns.startsWith(p))) continue;
      counts[l.ns] = (counts[l.ns] ?? 0) + 1;
      if (lines.length < limit) lines.push(l);
      else if (!more) {
        more = true;
        next = l.at_ms;
      }
    }
    const oldest = this.lines[this.linesHead]?.at_ms ?? null;
    return { reading: this.reading, oldest_ms: oldest, counts, more, next_from_ms: next, lines };
  }

  keepLine(ns, line) {
    const atMs = traceMs(/"at":"([^"]+)"/.exec(line.slice(0, 120))?.[1]);
    if (atMs == null) return;
    this.lines.push({ at_ms: atMs, ns, line: line.length > MAX_LINE_CHARS ? `${line.slice(0, MAX_LINE_CHARS)}…` : line });
    if (this.lines.length - this.linesHead > this.max.lines) {
      this.lines[this.linesHead++] = undefined;
      if (this.linesHead >= this.max.lines) {
        this.lines = this.lines.slice(this.linesHead);
        this.linesHead = 0;
      }
    }
  }

  /** The node's state from its trace; the lists only from `sinceMs` on when given. */
  node(sinceMs = null) {
    const after = (list) => (sinceMs == null ? list : list.filter((e) => e.at_ms >= sinceMs));
    const { mempool, bodyHits, arrivals, ledger } = this.state;
    return { reading: this.reading, mempool, body_hits: after(bodyHits), fetch_arrivals: after(arrivals), ledger };
  }

  /**
   * The mempool as a sketch: {reading, txs (as the trace tells it), node_txs
   * (as the node last said), at_ms (when it said so), since_ms (the earliest
   * trace time read: a tx added before it is missing), k, bottom: the k
   * smallest tx ids, cut, ascending}. With txs <= k the sketch is the whole
   * mempool.
   */
  sketch(k) {
    const bottom = [...this.pool.keys()].sort().slice(0, k).map((id) => id.slice(0, SKETCH_CHARS));
    const { mempool } = this.state;
    return {
      reading: this.reading,
      txs: this.pool.size,
      node_txs: mempool?.txs ?? null,
      at_ms: mempool?.at_ms ?? null,
      since_ms: this.firstMs,
      k,
      bottom,
    };
  }

  keep(list, entry) {
    list.push(entry);
    if (list.length > NODE_HISTORY) list.shift();
  }

  push(event) {
    event.seq = ++this.seq;
    this.log.push(event);
    const list = this.byTx.get(event.tx);
    if (list) list.push(event);
    else this.byTx.set(event.tx, [event]);
    if (this.log.length - this.head > this.max.events) {
      const gone = this.log[this.head];
      this.log[this.head++] = undefined;
      // Now and then drop the dead front, so the array stays about the bound.
      if (this.head >= this.max.events) {
        this.log = this.log.slice(this.head);
        this.head = 0;
      }
      const rest = this.byTx.get(gone.tx);
      rest.shift();
      if (!rest.length) this.byTx.delete(gone.tx);
      if ((this.watched.get(gone.tx) ?? 0) > this.now()) {
        const kept = this.kept.get(gone.tx);
        if (this.keptCount < MAX_KEPT && (kept?.length ?? 0) < MAX_KEPT_PER_TX) {
          if (kept) kept.push(gone);
          else this.kept.set(gone.tx, [gone]);
          this.keptCount++;
        }
      }
    }
  }

  /** Keep the events of `txIds` for `watchMs` from now, past the log's bound. */
  watch(txIds) {
    const now = this.now();
    for (const id of txIds) {
      this.watched.delete(id); // re-inserted at the end: the map stays oldest first
      this.watched.set(id, now + this.watchMs);
    }
    for (const [id, until] of this.watched) {
      if (until > now && this.watched.size <= this.max.watched) break;
      this.watched.delete(id);
      this.keptCount -= this.kept.get(id)?.length ?? 0;
      this.kept.delete(id);
    }
  }

  remember(map, key, value, cap) {
    map.set(key, value);
    if (map.size > cap) map.delete(map.keys().next().value);
  }

  /** One line of the trace. Lines of other namespaces are skipped without parsing. */
  ingest(line) {
    const at = line.indexOf('"ns":"');
    if (at < 0 || at > 80) return;
    const ns = line.slice(at + 6, line.indexOf('"', at + 6));
    if (logged(ns)) this.keepLine(ns, line);
    if (!KEPT.includes(ns)) return;
    let e;
    try {
      e = JSON.parse(line);
    } catch {
      return;
    }
    const base = { at: e.at, at_ms: traceMs(e.at) };
    if (base.at_ms != null && (this.firstMs == null || base.at_ms < this.firstMs)) this.firstMs = base.at_ms;
    const d = e.data ?? {};
    const idOf = (tx) => {
      const id = tx?.txIdFull ?? tx?.txIdentity?.ledgerTxId;
      const hash = tx?.txIdentity?.serializedTxHash;
      if (id && hash) this.remember(this.serialized, hash, id, this.max.hashes);
      return id;
    };
    if (ns === "Mempool.AddedTx") {
      const tx = idOf(d.tx);
      if (tx) this.pool.set(tx, base.at_ms);
    } else if (ns === "Mempool.RemoveTxs") {
      for (const r of d.txs ?? []) this.pool.delete(idOf(r.tx));
    }
    // Mempool events carry its size after them (when the node's tracer gives it).
    const size = d.mempoolSize;
    if (size && (ns === "Mempool.AddedTx" || ns === "Mempool.RemoveTxs")) {
      this.state.mempool = { at: e.at, at_ms: base.at_ms, txs: size.numTxs ?? null, bytes: size.bytes ?? null };
      for (const id of this.pool.keys()) {
        if (size.numTxs == null || this.pool.size <= size.numTxs) break;
        this.pool.delete(id);
      }
    }
    if (ns === "Consensus.LeiosKernel.BodyHits") {
      this.keep(this.state.bodyHits, {
        ...base,
        eb: d.ebHash ?? null,
        eb_slot: d.ebSlot ?? null,
        txs_in_eb: d.txsInEb ?? null,
        mempool_hits: d.mempoolHits ?? null,
        tracked: d.tracked ?? null,
        acquired: d.acquired ?? null,
        validated: d.validated ?? null,
        missed_both: d.missedBoth ?? null,
        cache_tx_count: d.cacheTxCount ?? null,
        cache_load: d.cacheLoad ?? null,
      });
      return;
    }
    if (ns === "Consensus.LeiosKernel.FetchTxsArrival") {
      this.keep(this.state.arrivals, {
        ...base,
        good_bytes: d.goodBytes ?? 0,
        extra_bytes: d.extraBytes ?? 0,
        evicted_bytes: d.evictedBytes ?? 0,
        invalid_bytes: d.invalidBytes ?? 0,
      });
      return;
    }
    if (ns === "LedgerMetrics") {
      this.state.ledger = { ...base, slot: d.slot ?? null, utxo_size: d.utxoSize ?? null, chain_density: d.chainDensity ?? null };
      return;
    }
    if (ns === "Mempool.AddedTx") {
      const tx = idOf(d.tx);
      if (tx) this.push({ ...base, kind: "added", tx });
    } else if (ns === "Mempool.RejectedTx") {
      const tx = idOf(d.tx);
      if (tx) this.push({ ...base, kind: "rejected", tx, reason: reasonOf(d.err) });
    } else if (ns === "Mempool.RemoveTxs") {
      for (const r of d.txs ?? []) {
        const tx = idOf(r.tx);
        if (tx) this.push({ ...base, kind: "removed", tx, reason: reasonOf(r.err) });
      }
    } else if (ns === "BlockFetch.Remote.Receive.Block") {
      const hash = d.msg?.blockHash;
      if (hash && !this.bodies.has(hash)) this.remember(this.bodies, hash, d.msg.txIds ?? [], this.max.blocks);
    } else if (ns === "ChainDB.AddBlockEvent.AddedToCurrentChain" || ns === "ChainDB.AddBlockEvent.SwitchedToAFork") {
      // A fork switch lists the new chain's blocks the same way: its last is the new tip.
      for (const h of d.headers ?? []) {
        const hash = String(h.hash ?? "").replaceAll('"', "");
        const block = { number: Number(h.blockNo), hash, slot: Number(h.slotNo) };
        this.tip = { ...block, at: e.at, at_ms: base.at_ms };
        for (const tx of this.bodies.get(hash) ?? []) this.push({ ...base, kind: "block", tx, block });
      }
    } else if (ns === "LeiosFetch.Remote.Receive.Block") {
      const eb = d.msg?.ebHash;
      for (const ref of d.msg?.txReferences ?? []) {
        const tx = this.serialized.get(ref.serializedTxHash);
        if (eb && tx) this.push({ ...base, kind: "eb", tx, eb });
      }
    }
  }

  /**
   * Events after cursor `since` (all kept events when null), only of `txIds`
   * when given, which are then watched. `gap` says events after `since` were
   * already dropped (a watched tx's events never are, while it is watched).
   */
  events(since, txIds = []) {
    const oldest = this.log[this.head]?.seq ?? this.seq + 1;
    let events;
    if (txIds.length) {
      this.watch(txIds);
      events = txIds
        .flatMap((id) => [...(this.kept.get(id) ?? []), ...(this.byTx.get(id) ?? [])])
        .filter((e) => since == null || e.seq > since);
      events.sort((a, b) => a.seq - b.seq);
    } else {
      const from = this.head + (since == null ? 0 : Math.max(0, since + 1 - oldest));
      events = this.log.slice(from, from + MAX_EVENTS_REPLY + 1);
    }
    const more = events.length > MAX_EVENTS_REPLY;
    if (more) events = events.slice(0, MAX_EVENTS_REPLY);
    const cursor = more ? events[events.length - 1].seq : this.seq;
    return { source: "trace", reading: this.reading, cursor, oldest, gap: since != null && since + 1 < oldest, more, tip: this.tip, events };
  }
}

/**
 * Follow the node's trace into `index`: read what is new every `everyMs`, and
 * when the file is rotated (TRACE_FILE is a symlink the node repoints), finish
 * the old file and start the new one from its beginning. A new bridge reads
 * the current file from the start, so it knows the last ~40 minutes. Returns
 * a function that stops it.
 */
export function followTrace(file, index, { everyMs = 500, chunk = 1 << 20, log = () => {} } = {}) {
  let current = null; // {path, handle, offset}
  let rest = Buffer.alloc(0);
  let stopped = false;
  let timer = null;

  async function drain() {
    const buffer = Buffer.alloc(chunk);
    for (;;) {
      const { bytesRead } = await current.handle.read(buffer, 0, chunk, current.offset);
      if (!bytesRead) return;
      current.offset += bytesRead;
      let data = Buffer.concat([rest, buffer.subarray(0, bytesRead)]);
      let start = 0;
      for (let nl = data.indexOf(10); nl >= 0; nl = data.indexOf(10, start)) {
        index.ingest(data.toString("utf8", start, nl));
        start = nl + 1;
      }
      rest = Buffer.from(data.subarray(start));
    }
  }

  async function step() {
    try {
      const path = await realpath(file);
      if (current && current.path !== path) {
        await drain().catch(() => {}); // what the old file got before the switch
        await current.handle.close().catch(() => {});
        current = null;
      }
      if (!current) {
        current = { path, handle: await open(path, "r"), offset: 0 };
        rest = Buffer.alloc(0);
        log(`reading node trace ${path}`);
      }
      const { size } = await current.handle.stat();
      if (size < current.offset) {
        current.offset = 0; // truncated in place
        rest = Buffer.alloc(0);
      }
      await drain();
      index.reading = true;
    } catch (error) {
      index.reading = false;
      log(`node trace: ${error.message}`);
    }
    if (!stopped) timer = setTimeout(step, everyMs);
  }

  step();
  return () => {
    stopped = true;
    clearTimeout(timer);
    current?.handle.close().catch(() => {});
  };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const cfg = config();
  const trace = cfg.traceFile ? new TraceIndex() : null;
  if (trace) followTrace(cfg.traceFile, trace, { log: (m) => console.log(m) });
  bridge(cfg, trace).listen(cfg.port, "127.0.0.1", () => {
    console.log(`relay bridge on http://127.0.0.1:${cfg.port}`);
    for (const [id, socket] of Object.entries(cfg.relays)) {
      console.log(`  ${id.padEnd(9)} ${socket} ${existsSync(socket) ? "open" : "missing"}`);
    }
  });
}
