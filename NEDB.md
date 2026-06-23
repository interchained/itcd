# NEDB Storage Backend

**itcd** — ITC daemon with the NEDB causal DAG engine replacing LevelDB.

---

## The Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                  ITC C++ Consensus Layer                     │
│  SHA-256 PoW · DGW-Nova difficulty · ITSL token subsystem   │
│  P2P protocol (port 17333) · Mempool · Script validation     │
└──────────────────────┬──────────────────────────────────────┘
                       │  CDBWrapper interface (unchanged API)
                       ▼
┌─────────────────────────────────────────────────────────────┐
│                 src/dbwrapper_nedb.cpp                       │
│  CDBWrapper shim — converts Bitcoin binary KV ops to NEDB   │
└──────────────────────┬──────────────────────────────────────┘
                       │  C FFI (nedb-ffi/nedb.h)
                       ▼
┌─────────────────────────────────────────────────────────────┐
│                   nedb-ffi/src/lib.rs                        │
│  Rust C API bridge (cbindgen)                               │
│  Phase 1: BTreeMap-backed in-process store                  │
│  Phase 2: nedb_core_v2::Db — BLAKE2b chain · MVCC · DAG    │
└──────────────────────┬──────────────────────────────────────┘
                       │  Rust → nedbd --dag
                       ▼
┌─────────────────────────────────────────────────────────────┐
│                   NEDB DAG Engine                            │
│  append-only log · BLAKE2b Merkle head · MVCC AS OF         │
│  caused_by causal chain · AES-256-GCM (NEDB_TMK)           │
│  nedbd HTTP :7070 · RESP2 :6380                             │
└──────────────────────┬──────────────────────────────────────┘
                       │  NQL queries
                       ▼
┌─────────────────────────────────────────────────────────────┐
│                   NEDB Studio                                │
│  Block explorer · TRACE provenance · AS OF time-travel      │
│  Chain verification · UTXO drill-down                       │
└─────────────────────────────────────────────────────────────┘
```

---

## Why NEDB Replaces LevelDB

| LevelDB (Bitcoin/ITC default) | NEDB DAG engine |
|-------------------------------|-----------------|
| Dumb key-value, no causal structure | `caused_by` — every write knows its parent |
| No history — current state only | `AS OF seq` — time-travel to any block height |
| Reorg = delete + rewrite UTXO set | Reorg = MVCC branch; old state survives intact |
| No state root (Bitcoin) | BLAKE2b chain head = deterministic state root |
| LevelDB XOR obfuscation | AES-256-GCM at-rest encryption (NEDB_TMK) |
| No tamper evidence | `verify()` — cryptographic hash chain proof |
| External block explorer needed | `TRACE caused_by` — provenance inside the node |

---

## Consensus From the Inside Out

In traditional Bitcoin nodes, consensus is a **network-layer property** — nodes
agree on the longest chain, and storage is a passive consequence.

With NEDB as storage, consensus becomes a **storage-layer invariant**:

- Every block PUT uses `caused_by: prev_block_hash` — causal ordering is encoded
  in the storage layer itself, not just the application layer.
- The BLAKE2b chain head after writing a block's transactions is deterministic:
  two nodes that processed the same chain arrive at the **same head**.
- This head is a state root — it can be stored in the block header or checked
  out-of-band, providing consensus verification without a block explorer.
- NEDB's append-only log means the node **cannot** accept a block that breaks
  causal ordering without the storage layer rejecting it.

---

## Genesis Block

The ITC mainnet genesis block is the root of the NEDB causal DAG.

```
hash    : 00000000ed361749ae598d60cd78395eb526bc90f5e1198f0b045f95cecc80c8
height  : 0
nTime   : 1751571776
nNonce  : 1176396455
nBits   : 0x1d00ffff
nVersion: 1
reward  : 50 ITC (unspendable coinbase)
merkle  : 4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b
P2P port: 17333
seeds   : seed.interchained.com, seed.interchained.org
```

In NEDB, the genesis block is stored as the root record with `caused_by: null`:

```json
{
  "_id":       "00000000ed361749ae598d60cd78395eb526bc90f5e1198f0b045f95cecc80c8",
  "height":    0,
  "prev_hash": "0000000000000000000000000000000000000000000000000000000000000000",
  "nTime":     1751571776,
  "nNonce":    1176396455,
  "nBits":     "1d00ffff",
  "nVersion":  1,
  "merkle":    "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b",
  "reward":    5000000000,
  "caused_by": null
}
```

Every subsequent block is `caused_by: <prev_block_hash>` — the blockchain IS the
NEDB causal DAG.

---

## CDBWrapper Seam

The single replacement point is `src/dbwrapper.cpp` → `src/dbwrapper_nedb.cpp`.

The public header `src/dbwrapper.h` retains the identical C++ API:
- `CDBWrapper(path, cacheSize, fMemory, fWipe, obfuscate)`
- `Read<K,V>`, `Write<K,V>`, `Exists<K>`, `Erase<K>`, `WriteBatch`
- `NewIterator()` → `CDBIterator` with `Valid/SeekToFirst/Seek/Next/GetKey/GetValue`
- `IsEmpty()`, `EstimateSize()` (stub), `CompactRange()` (no-op)
- **NEW**: `GetStateRoot()` — returns the BLAKE2b chain head hex string.

The LevelDB private members (`leveldb::DB* pdb`, `leveldb::Options`, etc.) are
replaced by a single `NedbHandle* pdb`.

---

## Build Phases

### Phase 1 (this PR)
- `nedb-ffi` Rust crate with BTreeMap-backed HashMap store.
- `src/dbwrapper.h` + `src/dbwrapper_nedb.cpp` shim compiles in place of LevelDB.
- All existing ITC consensus tests pass.

### Phase 2 (next PR)
- Wire `nedb_core_v2::Db` into `nedb-ffi/src/lib.rs`.
- Remove the BTreeMap; every write goes to the DAG engine.
- BLAKE2b state root is computed per-block and stored alongside the block index.
- `NEDB_TMK` environment variable enables AES-256-GCM at-rest encryption.

### Phase 3 (future)
- Semantic collections: `blocks`, `txs`, `utxos` in NEDB instead of binary KV.
- NEDB Studio block explorer panel queries the live node via NQL.
- `AS OF <height>` — any UTXO state at any historical block — from the node itself.
- `TRACE caused_by` — walk any UTXO's causal chain back to genesis inside the node.

---

## NEDB Studio Integration

The NEDB Studio (studio.interchained.org) connects to the same `nedbd --dag`
instance that the node writes to. Once Phase 2 is wired:

```
NQL: FROM blocks AS OF 100000 WHERE height = 100000
NQL: FROM utxos TRACE caused_by WHERE _id = "<txid>:<vout>"
NQL: FROM blocks VALID AS OF "2025-01-01" WHERE height > 500000
```

The node becomes its own block explorer — no external service required.

---

## Proof-of-Prefix Warm Boot

A node compiled today (Node B) holds its chain in the NEDB causal DAG, not in a
LevelDB blob it must replay. That changes startup from "scan and rebuild the
block index" into a **cryptographic proof**:

```
Node A  — seed.interchained.org:17101, online since genesis (the canonical chain)
Node B  — compiled today, has blocks 0..T persisted in NEDB
```

**Step 1 — Local integrity (content-addressed, read-time by default).**
NEDB objects are content-addressed: every read re-hashes the bytes and fails
loud on a BLAKE2b mismatch (`nedb-v2/src/store.rs::read`), so a tampered or
corrupt object can never be silently served — it is caught the instant it is
touched. Loading Node B's `base..T` window (and replaying its chainstate) thus
self-verifies what the node resumes from, with no separate scan. The eager
full-store re-hash — `Db::verify()` (exposed as `nedb_verify()` →
`CDBWrapper::Verify()`), which walks every object and confirms each still hashes
to its address — is O(n) and **OFF by default**; run it with **`-verifynedb`**
(e.g. after suspected on-disk corruption), in which case a failure wipes the
index and resyncs from height 0.

**Step 2 — Canonical seam (one comparison).**
Node B pins Node A as a persistent peer and confirms A's canonical chain
*contains* B's tip `T` — either A extends `T`, or A sends a range that includes
`T`'s hash. Because Step 1 proved every block below `T` is fixed by the
back-links, a single confirmation of the tip proves the **entire prefix**. No
block-by-block comparison, no Bitcoin-Core skip-list ancestor walk.

**Step 3 — Sync forward, or fall back to 0.**
Seam closed → warm boot confirmed; IBD proceeds forward from `T`. Seam never
closes within the watchdog deadline, or A serves a chain that does not contain
`T` → B's tip is not canonical → full resync from height 0.

```
verify() intact  +  A's chain ∋ tip T   ⟹   B's chain 0..T == A's chain 0..T
```

This is what "the blockchain IS the NEDB causal DAG" buys at startup: integrity
is a storage-layer property (`verify()`), and consensus membership collapses to
a single tip check. The legacy assumption that the in-memory block index is
always fully linked is gone — `LastCommonAncestor` and the ancestor walk treat a
partial index (the rest still in the DAG) as normal, never as a fatal assert.

> The consensus FFI deliberately stays a small set of typed primitives
> (`get/put/del/batch/head/scan/verify`). NEDB's NQL query language is **not**
> exposed across this seam — rich querying belongs at the nedbd HTTP layer that
> NEDB Studio and the explorer already use.

---

*© Interchained LLC × Claude Sonnet 4.6*
