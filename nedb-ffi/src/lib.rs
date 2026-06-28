//! nedb-ffi: C API bridge for the NEDB DAG engine
//!
//! This crate exposes NEDB's causal DAG storage to the ITC C++ node,
//! replacing LevelDB as the block index and chainstate backend.
//!
//! # Phase 1 (feature = "phase1" or no features)
//! BTreeMap-backed in-process store. Proves the C FFI surface.
//!
//! # Phase 2 (feature = "phase2", default)
//! nedb_core_v2::Db — content-addressed DAG with real BLAKE2b-512 Merkle
//! chain head, MVCC AS OF, causal provenance (caused_by), and optional
//! AES-256-GCM encryption via NEDB_TMK env var.
//!
//! © Interchained LLC × Claude Sonnet 4.6

#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::missing_safety_doc)]

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_uchar};

// ── Phase 1 imports ───────────────────────────────────────────────────────────
#[cfg(not(feature = "phase2"))]
use {
    blake2::{Blake2b512, Digest},
    std::collections::BTreeMap,
    std::sync::Mutex,
};

// ── Phase 2 imports ───────────────────────────────────────────────────────────
#[cfg(feature = "phase2")]
use {
    nedb_engine::{Db, Dek},
    serde_json::json,
    std::collections::HashMap,
    std::path::Path,
    std::sync::Arc,
    std::sync::Mutex,
    std::sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

// ─────────────────────────────────────────────────────────────────────────────
// Phase 1: BTreeMap store
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(not(feature = "phase2"))]
struct NedbInner {
    store: BTreeMap<Vec<u8>, Vec<u8>>,
    seq: u64,
    head: Vec<u8>,
}

#[cfg(not(feature = "phase2"))]
impl NedbInner {
    fn advance_head(&mut self, key: &[u8], value: Option<&[u8]>) {
        let mut h = Blake2b512::new();
        h.update(&self.head);
        h.update(key);
        if let Some(v) = value { h.update(v); }
        h.update(self.seq.to_le_bytes());
        self.head = h.finalize().to_vec();
        self.seq += 1;
    }
}

#[cfg(not(feature = "phase2"))]
pub struct NedbHandle { inner: Mutex<NedbInner> }

// ─────────────────────────────────────────────────────────────────────────────
// Phase 2: nedb_core_v2::Db handle
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(feature = "phase2")]
pub struct NedbHandle {
    db:     Arc<Db>,
    coll:   String,
    path:   std::path::PathBuf,                   // database root directory for direct file access
    stop:   Arc<AtomicBool>,                      // signals the flush ticker to exit
    ticker: Option<std::thread::JoinHandle<()>>,  // FFI-managed flush thread, joined on close
    // When false, writes skip the per-key read-before-write that derives
    // caused_by (provenance). Set via nedb_set_provenance() for lookup-table
    // databases whose causal lineage is already intrinsic to the payload — e.g.
    // the block index, where every CDiskBlockIndex carries hashPrev. The engine
    // still tracks each entry's `prev` from the in-memory id_index (free) and
    // get() resolves the current value via that index, so latest-write-wins is
    // unchanged; we only drop the FFI's redundant full-object disk load that
    // existed solely to copy a hash into caused_by. NOTE: never disable this for
    // the chainstate — its caused_by IS the consensus UTXO causal history.
    provenance:   AtomicBool,
    // Diagnostics: number of provenance reads avoided (writes issued while
    // provenance was disabled). Surfaced via nedb_reads_sliced().
    reads_sliced: AtomicU64,
    // Write-dedup shadow map (no-provenance DBs only): id → BLAKE2b of the last
    // value we durably wrote for that id. A write whose value hashes identically
    // is skipped — no object write, no id-index churn, no Merkle advance, and no
    // disk read. SAFETY/consistency with the engine's id_index: an entry is set
    // ONLY after put_batch/put succeeds, evicted on delete, and the map starts
    // empty each process. So a skip can only happen when the engine provably
    // already holds that exact value (we wrote it earlier this session), and the
    // engine's current-value pointer is never left missing a real update. Bounded
    // to lookup-table DBs (the block index) where redundant rewrites dominate;
    // the chainstate (provenance on) never populates this and is unaffected.
    write_cache:    Mutex<HashMap<String, [u8; 32]>>,
    writes_skipped: AtomicU64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Iterator — same shape in both phases (decoded binary snapshot)
// ─────────────────────────────────────────────────────────────────────────────

pub struct NedbIter {
    entries: Vec<(Vec<u8>, Vec<u8>)>,
    pos:     usize,
}

// ─────────────────────────────────────────────────────────────────────────────
// Batch op
// ─────────────────────────────────────────────────────────────────────────────

#[repr(C)]
pub struct NedbOp {
    pub key:       *const c_uchar,
    pub key_len:   usize,
    pub value:     *const c_uchar,
    pub value_len: usize,
}

// ─────────────────────────────────────────────────────────────────────────────
// C API — database lifecycle
// ─────────────────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn nedb_open(path: *const c_char, _dek: *const c_char) -> *mut NedbHandle {
    if path.is_null() { return std::ptr::null_mut(); }

    #[cfg(not(feature = "phase2"))]
    {
        let _path = unsafe { CStr::from_ptr(path) };
        Box::into_raw(Box::new(NedbHandle {
            inner: Mutex::new(NedbInner {
                store: BTreeMap::new(),
                seq:   0,
                head:  vec![0u8; 64],
            }),
        }))
    }

    #[cfg(feature = "phase2")]
    {
        let path_str = unsafe { CStr::from_ptr(path).to_string_lossy() };
        let db_path  = Path::new(path_str.as_ref());
        let coll = db_path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "itc".to_string());

        let dek_opt: Option<Dek> = std::env::var("NEDB_TMK")
            .ok()
            .and_then(|s| hex::decode(&s).ok())
            .and_then(|b| b.try_into().ok())
            .map(Dek);

        let db = match Db::open(db_path, dek_opt) {
            Ok(d)  => d,
            Err(_) => return std::ptr::null_mut(),
        };
        let db_arc = Arc::new(db);
        Db::start_cold_scan(Arc::clone(&db_arc));

        // FFI-managed, STOPPABLE flush ticker (replaces Db::start_manifest_ticker).
        // The engine's built-in ticker is an infinite `loop {}` with no stop
        // signal that holds its own Arc<Db>. With one per open database it kept
        // flushing MANIFEST during shutdown and raced the final flush + Db
        // teardown — segfaulting the node on Ctrl+C. nedb_close() now signals
        // `stop` and JOINS this thread before the final flush, so no background
        // thread can ever touch the Db during teardown. Poll every 100ms (clean,
        // prompt shutdown); flush the WAL + MANIFEST every ~5s as before.
        let stop   = Arc::new(AtomicBool::new(false));
        let stop_t = Arc::clone(&stop);
        let db_t   = Arc::clone(&db_arc);
        let ticker = std::thread::spawn(move || {
            let mut elapsed_ms: u64 = 0;
            while !stop_t.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(100));
                elapsed_ms += 100;
                if elapsed_ms >= 5_000 {
                    elapsed_ms = 0;
                    db_t.flush_all();
                }
            }
        });

        Box::into_raw(Box::new(NedbHandle {
            db: db_arc, coll, path: db_path.to_path_buf(),
            stop, ticker: Some(ticker),
            provenance:   AtomicBool::new(true),  // on by default; opt-out per DB
            reads_sliced: AtomicU64::new(0),
            write_cache:    Mutex::new(HashMap::new()),
            writes_skipped: AtomicU64::new(0),
        }))
    }
}

#[no_mangle]
pub extern "C" fn nedb_close(handle: *mut NedbHandle) {
    if handle.is_null() { return; }

    // Take ownership so background work can be stopped before the Db is dropped.
    #[allow(unused_mut)]
    let mut boxed = unsafe { Box::from_raw(handle) };

    #[cfg(feature = "phase2")]
    {
        // Shutdown-safety invariant: no background thread may touch the Db while
        // we flush and drop it. Stop the flush ticker and JOIN it first (polling
        // is 100ms, so the join returns promptly), THEN do one final flush of the
        // id-index WAL + MANIFEST. Without this the immortal ticker raced the
        // teardown and segfaulted the node on Ctrl+C.
        boxed.stop.store(true, Ordering::Relaxed);
        if let Some(t) = boxed.ticker.take() {
            let _ = t.join();
        }
        boxed.db.flush_all();
    }

    drop(boxed);
}

/// Enable (1) or disable (0) the NEDB v3 segment/pack object store for databases
/// opened AFTER this call. **Must be called BEFORE nedb_open()** — the engine
/// selects its object substrate when the store is constructed at open time
/// (it reads the process-global `NEDB_DAG_V3` switch there). Process-global by
/// design: the ITC node calls this once at startup (via the `-dagv3` arg) so the
/// block index AND the chainstate both run on segments.
///
/// v3 batches the default one-file-per-object loose store into append-only
/// segment packs — one fsync per group-commit instead of one per object — with
/// background compaction and `.idx` sidecars, which is what makes the chainstate
/// / block-index flush fast during IBD. It is TRANSPARENT to everything this FFI
/// exposes: keys, values, get/put/batch, the BLAKE2b Merkle head, AS OF, and
/// causal provenance are all unchanged. Existing v2 loose objects stay readable
/// via the engine's dual-read fallback, so flipping this on is non-destructive.
/// Default: off (v2 loose objects) — this is a pure opt-in.
#[no_mangle]
pub extern "C" fn nedb_set_dag_v3(enabled: c_int) {
    #[cfg(feature = "phase2")]
    {
        // The ITC node calls this during single-threaded startup, before any
        // nedb_open(), so the set is observed by the engine when it constructs
        // the object store. (The engine re-reads the switch at every open, so it
        // also covers the second DB opened in-process.)
        if enabled != 0 {
            std::env::set_var("NEDB_DAG_V3", "1");
        } else {
            std::env::remove_var("NEDB_DAG_V3");
        }
    }
    #[cfg(not(feature = "phase2"))]
    {
        let _ = enabled; // Phase 1 (in-process map) has no on-disk substrate.
    }
}

/// Enable (1) or disable (0) fast fsync for the NEDB v3 segment store. **Must be
/// called BEFORE nedb_open()** — the engine reads the process-global
/// `NEDB_FAST_FSYNC` switch when it constructs the segment store. The ITC node
/// calls this once at startup via the `-dagfastsync` arg.
///
/// When on, the engine uses a plain `fsync(2)` at v3 durability points instead of
/// the OS full barrier (`F_FULLFSYNC` on macOS) — much faster flush on macOS
/// (Fusion/SATA), still crash-safe, at the cost of power-loss-to-platter
/// durability (the chainstate is reconstructible from peers). Requires the v3
/// store (`-dagv3`); a no-op on Linux/Windows, where the default sync is already
/// a plain fsync. Default: off.
#[no_mangle]
pub extern "C" fn nedb_set_fast_fsync(enabled: c_int) {
    #[cfg(feature = "phase2")]
    {
        if enabled != 0 {
            std::env::set_var("NEDB_FAST_FSYNC", "1");
        } else {
            std::env::remove_var("NEDB_FAST_FSYNC");
        }
    }
    #[cfg(not(feature = "phase2"))]
    {
        let _ = enabled; // Phase 1 (in-process map) has no on-disk fsync.
    }
}

/// Enable (1) or disable (0) causal provenance for writes on this database.
///
/// Default is enabled. Disable it ONLY for lookup-table databases whose causal
/// lineage is already intrinsic to the stored payload — the block index is the
/// canonical case: every CDiskBlockIndex carries hashPrev, so the block→parent
/// edge lives in the data and NEDB's per-entry caused_by (which merely chains an
/// entry to its own previous version) is redundant bookkeeping nobody consumes.
///
/// With provenance off, a write skips the read-before-write that loads the prior
/// object from disk just to copy its hash into caused_by. The engine still sets
/// each node's `prev` from the in-memory id_index (free) and get() resolves the
/// current value through that index, so latest-write-wins is unchanged. Only the
/// caused_by DAG edges (TRACE) are omitted for this DB.
///
/// NEVER disable this for the chainstate: there caused_by IS the consensus UTXO
/// causal history. Block index and chainstate are separate NEDB databases.
#[no_mangle]
pub extern "C" fn nedb_set_provenance(handle: *mut NedbHandle, enabled: c_int) {
    if handle.is_null() { return; }
    #[cfg(feature = "phase2")]
    {
        unsafe { &*handle }.provenance.store(enabled != 0, Ordering::Relaxed);
    }
    #[cfg(not(feature = "phase2"))]
    {
        let _ = enabled; // Phase 1 (in-process map) has no causal provenance.
    }
}

/// Number of provenance reads avoided so far (writes issued with provenance off).
/// Diagnostic only — lets the node log how much read-before-write work the block
/// index flush skipped. Returns 0 in Phase 1.
#[no_mangle]
pub extern "C" fn nedb_reads_sliced(handle: *mut NedbHandle) -> u64 {
    if handle.is_null() { return 0; }
    #[cfg(feature = "phase2")]
    {
        unsafe { &*handle }.reads_sliced.load(Ordering::Relaxed)
    }
    #[cfg(not(feature = "phase2"))]
    {
        0
    }
}

/// Number of redundant writes skipped by the write-dedup shadow map (values
/// rewritten with byte-identical content). Diagnostic only; 0 in Phase 1.
#[no_mangle]
pub extern "C" fn nedb_writes_skipped(handle: *mut NedbHandle) -> u64 {
    if handle.is_null() { return 0; }
    #[cfg(feature = "phase2")]
    {
        unsafe { &*handle }.writes_skipped.load(Ordering::Relaxed)
    }
    #[cfg(not(feature = "phase2"))]
    {
        0
    }
}

/// BLAKE2b-256 fingerprint of a value's bytes — the write-dedup key. Independent
/// of the engine's per-Node content hash (which folds in seq/ts and so differs
/// for identical values); this fingerprints the payload alone, so a re-written
/// value matches its predecessor and can be skipped.
#[cfg(feature = "phase2")]
fn dedup_fingerprint(bytes: &[u8]) -> [u8; 32] {
    use blake2::{Blake2b512, Digest};
    let mut hasher = Blake2b512::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut fp = [0u8; 32];
    fp.copy_from_slice(&out[..32]);
    fp
}

// ─────────────────────────────────────────────────────────────────────────────
// C API — single-record operations
// ─────────────────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn nedb_get(
    handle: *mut NedbHandle,
    key: *const c_uchar, key_len: usize,
    value_out: *mut *mut c_uchar, value_len_out: *mut usize,
) -> c_int {
    if handle.is_null() || key.is_null() { return -1; }

    #[cfg(not(feature = "phase2"))]
    {
        let inner    = unsafe { &*handle }.inner.lock().unwrap();
        let key_bytes = unsafe { std::slice::from_raw_parts(key, key_len) };
        match inner.store.get(key_bytes) {
            None => 1,
            Some(val) => {
                let mut boxed: Box<[u8]> = val.clone().into_boxed_slice();
                unsafe { *value_len_out = boxed.len(); *value_out = boxed.as_mut_ptr(); std::mem::forget(boxed); }
                0
            }
        }
    }

    #[cfg(feature = "phase2")]
    {
        let h = unsafe { &*handle };
        let key_hex = hex::encode(unsafe { std::slice::from_raw_parts(key, key_len) });
        match h.db.get(&h.coll, &key_hex) {
            None => 1,
            Some(node) => {
                let val_str = node.data["v"].as_str().unwrap_or("");
                match hex::decode(val_str) {
                    Err(_) => -1,
                    Ok(bytes) => {
                        let mut boxed: Box<[u8]> = bytes.into_boxed_slice();
                        unsafe { *value_len_out = boxed.len(); *value_out = boxed.as_mut_ptr(); std::mem::forget(boxed); }
                        0
                    }
                }
            }
        }
    }
}

#[no_mangle]
pub extern "C" fn nedb_free_value(ptr: *mut c_uchar, len: usize) {
    if !ptr.is_null() && len > 0 {
        unsafe { drop(Vec::from_raw_parts(ptr, len, len)) }
    }
}

/// Batch read: fetch N values by key, in PARALLEL on phase 2 (rayon over the
/// engine's get — mirrors the parallel put_batch). The read-side twin of
/// put_batch for the connect-loop coin prefetch: instead of N scattered
/// single-key NEDB reads interleaved with validation, the chainstate can warm a
/// whole batch of coins in one call.
///
/// Inputs reuse the `NedbOp` array — only `key`/`key_len` are read; `value`/
/// `value_len` are ignored. The caller pre-allocates two output arrays of length
/// `ops_len`: on return `values_out[i]` is a freshly-allocated buffer (free with
/// nedb_free_value) or null when the key is absent, and `value_lens_out[i]` is
/// its length (0 when absent). Returns 0 on success, -1 on a null-argument error.
#[no_mangle]
pub extern "C" fn nedb_get_batch(
    handle: *mut NedbHandle,
    ops: *const NedbOp, ops_len: usize,
    values_out: *mut *mut c_uchar,
    value_lens_out: *mut usize,
) -> c_int {
    if handle.is_null() || ops.is_null() || values_out.is_null() || value_lens_out.is_null() {
        return -1;
    }
    if ops_len == 0 { return 0; }
    let ops_slice = unsafe { std::slice::from_raw_parts(ops, ops_len) };

    // Materialize the keys up front (owned) so the parallel read has Sync data.
    let keys: Vec<Option<Vec<u8>>> = ops_slice.iter().map(|op| {
        if op.key.is_null() { None }
        else { Some(unsafe { std::slice::from_raw_parts(op.key, op.key_len) }.to_vec()) }
    }).collect();

    let results: Vec<Option<Vec<u8>>> = {
        #[cfg(feature = "phase2")]
        {
            use rayon::prelude::*;
            let h = unsafe { &*handle };
            keys.par_iter().map(|k| {
                let k = k.as_ref()?;
                let key_hex = hex::encode(k);
                let node = h.db.get(&h.coll, &key_hex)?;
                let val_str = node.data["v"].as_str()?;
                hex::decode(val_str).ok()
            }).collect()
        }
        #[cfg(not(feature = "phase2"))]
        {
            let inner = unsafe { &*handle }.inner.lock().unwrap();
            keys.iter().map(|k| {
                let k = k.as_ref()?;
                inner.store.get(k).cloned()
            }).collect()
        }
    };

    // Hand each buffer back to C (forget the box; caller frees with nedb_free_value).
    for (i, r) in results.into_iter().enumerate() {
        match r {
            Some(bytes) => {
                let mut boxed: Box<[u8]> = bytes.into_boxed_slice();
                unsafe {
                    *value_lens_out.add(i) = boxed.len();
                    *values_out.add(i)     = boxed.as_mut_ptr();
                }
                std::mem::forget(boxed);
            }
            None => unsafe {
                *value_lens_out.add(i) = 0;
                *values_out.add(i)     = std::ptr::null_mut();
            },
        }
    }
    0
}

#[no_mangle]
pub extern "C" fn nedb_put(
    handle: *mut NedbHandle,
    key: *const c_uchar, key_len: usize,
    value: *const c_uchar, value_len: usize,
) -> c_int {
    if handle.is_null() || key.is_null() || value.is_null() { return -1; }

    #[cfg(not(feature = "phase2"))]
    {
        let mut inner  = unsafe { &*handle }.inner.lock().unwrap();
        let key_bytes  = unsafe { std::slice::from_raw_parts(key,   key_len)   }.to_vec();
        let val_bytes  = unsafe { std::slice::from_raw_parts(value, value_len) }.to_vec();
        inner.advance_head(&key_bytes, Some(&val_bytes));
        inner.store.insert(key_bytes, val_bytes);
        0
    }

    #[cfg(feature = "phase2")]
    {
        let h = unsafe { &*handle };
        let key_bytes = unsafe { std::slice::from_raw_parts(key,   key_len)   };
        let val_bytes = unsafe { std::slice::from_raw_parts(value, value_len) };
        let key_id  = hex::encode(key_bytes);
        let val_hex = hex::encode(val_bytes);
        let provenance = h.provenance.load(Ordering::Relaxed);

        if !provenance {
            // No-provenance fast path (e.g. block index): skip the caused_by
            // read entirely, and skip the write too if the value is byte-
            // identical to what we last durably wrote for this key.
            let fp = dedup_fingerprint(val_bytes);
            if h.write_cache.lock().unwrap().get(&key_id) == Some(&fp) {
                h.writes_skipped.fetch_add(1, Ordering::Relaxed);
                return 0;
            }
            h.reads_sliced.fetch_add(1, Ordering::Relaxed);
            let data = json!({ "v": val_hex });
            match h.db.put(&h.coll, &key_id, data, Vec::new(), None, None) {
                Ok(_)  => {
                    // Commit the fingerprint only after the durable write.
                    h.write_cache.lock().unwrap().insert(key_id, fp);
                    0
                }
                Err(_) => -1,
            }
        } else {
            // Provenance on (e.g. chainstate): derive caused_by from the prior
            // version's content hash — unchanged behavior.
            let caused_by = h.db.get(&h.coll, &key_id)
                .filter(|n| !n.hash.is_empty())
                .map(|n| vec![n.hash.clone()])
                .unwrap_or_default();
            let data = json!({ "v": val_hex });
            match h.db.put(&h.coll, &key_id, data, caused_by, None, None) {
                Ok(_)  => 0,
                Err(_) => -1,
            }
        }
    }
}

#[no_mangle]
pub extern "C" fn nedb_del(handle: *mut NedbHandle, key: *const c_uchar, key_len: usize) -> c_int {
    if handle.is_null() || key.is_null() { return -1; }

    #[cfg(not(feature = "phase2"))]
    {
        let mut inner = unsafe { &*handle }.inner.lock().unwrap();
        let key_bytes = unsafe { std::slice::from_raw_parts(key, key_len) }.to_vec();
        inner.advance_head(&key_bytes, None);
        inner.store.remove(&key_bytes);
        0
    }

    #[cfg(feature = "phase2")]
    {
        let h      = unsafe { &*handle };
        let key_id = hex::encode(unsafe { std::slice::from_raw_parts(key, key_len) });
        // Evict from the write-dedup shadow map: after a delete the engine no
        // longer holds this value, so a later identical write must NOT be skipped.
        h.write_cache.lock().unwrap().remove(&key_id);
        match h.db.delete(&h.coll, &key_id) { Ok(_) => 0, Err(_) => -1 }
    }
}

#[no_mangle]
pub extern "C" fn nedb_exists(handle: *mut NedbHandle, key: *const c_uchar, key_len: usize) -> c_int {
    if handle.is_null() || key.is_null() { return -1; }

    #[cfg(not(feature = "phase2"))]
    {
        let inner     = unsafe { &*handle }.inner.lock().unwrap();
        let key_bytes = unsafe { std::slice::from_raw_parts(key, key_len) };
        if inner.store.contains_key(key_bytes) { 1 } else { 0 }
    }

    #[cfg(feature = "phase2")]
    {
        let h      = unsafe { &*handle };
        let key_id = hex::encode(unsafe { std::slice::from_raw_parts(key, key_len) });
        if h.db.get(&h.coll, &key_id).is_some() { 1 } else { 0 }
    }
}

#[no_mangle]
pub extern "C" fn nedb_is_empty(handle: *mut NedbHandle) -> c_int {
    if handle.is_null() { return -1; }

    #[cfg(not(feature = "phase2"))]
    { let inner = unsafe { &*handle }.inner.lock().unwrap(); if inner.store.is_empty() { 1 } else { 0 } }

    #[cfg(feature = "phase2")]
    { let h = unsafe { &*handle }; if h.db.list(&h.coll).is_empty() { 1 } else { 0 } }
}

// ─────────────────────────────────────────────────────────────────────────────
// C API — batch
// ─────────────────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn nedb_batch_write(handle: *mut NedbHandle, ops: *const NedbOp, ops_len: usize) -> c_int {
    if handle.is_null() || ops.is_null() { return -1; }
    let ops_slice = unsafe { std::slice::from_raw_parts(ops, ops_len) };

    #[cfg(not(feature = "phase2"))]
    {
        let mut inner = unsafe { &*handle }.inner.lock().unwrap();
        for op in ops_slice {
            if op.key.is_null() { continue; }
            let k = unsafe { std::slice::from_raw_parts(op.key, op.key_len) }.to_vec();
            if op.value.is_null() {
                inner.advance_head(&k, None);
                inner.store.remove(&k);
            } else {
                let v = unsafe { std::slice::from_raw_parts(op.value, op.value_len) }.to_vec();
                inner.advance_head(&k, Some(&v));
                inner.store.insert(k, v);
            }
        }
        0
    }

    #[cfg(feature = "phase2")]
    {
        use rayon::prelude::*;
        let h = unsafe { &*handle };
        let db   = &h.db;
        let coll: &str = &h.coll;

        // Copy key/value bytes into owned buffers first. The raw C pointers are
        // not Send, so they cannot be touched from rayon worker threads; the
        // owned Vecs can.
        //
        // CRITICAL — LevelDB batch semantics: operations apply IN ORDER and the
        // LAST op on a given key wins. CCoinsViewDB::BatchWrite depends on this:
        // in ONE batch it both Erases AND Writes the 2-phase-commit marker keys
        // (DB_BEST_BLOCK 'B', DB_HEAD_BLOCKS 'H') — Erase(B)+Write(H) … then
        // Erase(H)+Write(B). The earlier implementation split the batch into an
        // all-deletes pass followed by an all-puts pass, which made the PUT win
        // for both keys regardless of their true order — so DB_HEAD_BLOCKS stayed
        // populated when it should have been erased. That desynced the chainstate
        // commit markers and produced the BatchWrite (txdb.cpp:95) abort and the
        // downstream FindMostWorkChain (validation.cpp:2797) abort. Fix: collapse
        // the batch to net per-key state in op order (a later op supersedes an
        // earlier one). After this every key lands in exactly one of the
        // delete/put sets, so the two passes can never race on the same key.
        let mut idx: std::collections::HashMap<String, usize> =
            std::collections::HashMap::with_capacity(ops_slice.len());
        let mut resolved: Vec<(String, Option<Vec<u8>>)> = Vec::with_capacity(ops_slice.len());
        for op in ops_slice {
            if op.key.is_null() { continue; }
            let k   = unsafe { std::slice::from_raw_parts(op.key, op.key_len) };
            let kid = hex::encode(k);
            let val = if op.value.is_null() {
                None
            } else {
                Some(unsafe { std::slice::from_raw_parts(op.value, op.value_len) }.to_vec())
            };
            match idx.get(&kid) {
                Some(&i) => resolved[i].1 = val,                 // same key again → last op wins
                None     => { idx.insert(kid.clone(), resolved.len()); resolved.push((kid, val)); }
            }
        }
        let provenance = h.provenance.load(Ordering::Relaxed);
        // No-provenance lookup-table DBs (the block index) also get write-dedup:
        // a value re-written with byte-identical content is skipped entirely —
        // no object write, no id-index churn, no Merkle advance, no disk read.
        let dedup = !provenance;

        let mut puts_raw: Vec<(String, Vec<u8>)> = Vec::with_capacity(resolved.len());
        let mut del_ids:  Vec<String>            = Vec::new();
        // Fingerprints to commit to the shadow map AFTER the batch is durably
        // written (never before — see the write_cache safety note on NedbHandle).
        let mut pending_fp: Vec<(String, [u8; 32])> = Vec::new();
        let mut skipped: u64 = 0;

        if dedup {
            // cs_main is held by the caller, so there is no concurrent writer.
            // Reads decide skips; deletes evict immediately (always safe — worst
            // case a later write simply isn't deduped); put fingerprints are
            // staged and committed only once the write is durable.
            let mut cache = h.write_cache.lock().unwrap();
            for (kid, val) in resolved {
                match val {
                    Some(v) => {
                        let fp = dedup_fingerprint(&v);
                        if cache.get(&kid) == Some(&fp) {
                            skipped += 1; // identical bytes already stored → skip
                        } else {
                            pending_fp.push((kid.clone(), fp));
                            puts_raw.push((kid, v));
                        }
                    }
                    None => { cache.remove(&kid); del_ids.push(kid); }
                }
            }
        } else {
            for (kid, val) in resolved {
                match val {
                    Some(v) => puts_raw.push((kid, v)),
                    None    => del_ids.push(kid),
                }
            }
        }

        // Build the put ops IN PARALLEL. The provenance read-before-write (the
        // per-entry db.get() that derives caused_by) is skipped for DBs that
        // opted out via nedb_set_provenance — e.g. the block index, whose causal
        // lineage is already in the payload via hashPrev. Each db.get() is a full
        // content-addressed object load from disk (no read cache) solely to copy
        // a hash into caused_by, and it dominated the block-index flush. The
        // engine still sets node.prev from the in-memory id_index, so
        // get()/latest-write-wins is unchanged; only the caused_by DAG edges are
        // omitted for this (non-consensus) database.
        let put_ops: Vec<(String, String, serde_json::Value, Vec<String>, Option<String>, Option<String>)> =
            puts_raw.par_iter().map(|(kid, val)| {
                let caused_by = if provenance {
                    db.get(coll, kid)
                        .filter(|n| !n.hash.is_empty())
                        .map(|n| vec![n.hash.clone()])
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                (coll.to_string(), kid.clone(), json!({ "v": hex::encode(val) }), caused_by, None, None)
            }).collect();
        if !provenance {
            h.reads_sliced.fetch_add(puts_raw.len() as u64, Ordering::Relaxed);
        }

        // Deletes (spent coins) — tombstones. Parallelised too. Keys here are now
        // guaranteed disjoint from the puts above (the batch was collapsed to net
        // per-key state in op order), so the delete pass and the put pass can
        // never operate on the same key.
        if !del_ids.is_empty() {
            del_ids.par_iter().for_each(|kid| { let _ = db.delete(coll, kid); });
        }

        // put_batch writes all content-addressed objects in parallel (rayon),
        // assigns one contiguous seq range, and chains the caused_by DAG edges +
        // Merkle head in a single pass. This replaces N serial single-puts.
        if !put_ops.is_empty() {
            if db.put_batch(put_ops).is_err() { return -1; }
        }

        // One durability point per batch — flush id-index WAL + MANIFEST.
        db.flush_all();

        // Commit dedup fingerprints only now that the writes are durable, so the
        // shadow map can never claim a value the engine does not actually hold.
        if dedup {
            if !pending_fp.is_empty() {
                let mut cache = h.write_cache.lock().unwrap();
                for (kid, fp) in pending_fp { cache.insert(kid, fp); }
            }
            if skipped > 0 {
                h.writes_skipped.fetch_add(skipped, Ordering::Relaxed);
            }
        }
        0
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// C API — state root (BLAKE2b chain head)
// ─────────────────────────────────────────────────────────────────────────────

/// Returns the current BLAKE2b state root as a hex string.
/// Phase 1: BLAKE2b-512 chain (128 hex chars), advances on every write.
/// Phase 2: NEDB MANIFEST Merkle root — deterministic, identical across nodes
///          that have processed the same ITC chain. This IS the consensus proof.
#[no_mangle]
pub extern "C" fn nedb_head(handle: *mut NedbHandle) -> *mut c_char {
    if handle.is_null() { return CString::new("").unwrap().into_raw(); }

    #[cfg(not(feature = "phase2"))]
    {
        let inner = unsafe { &*handle }.inner.lock().unwrap();
        CString::new(hex::encode(&inner.head)).unwrap().into_raw()
    }

    #[cfg(feature = "phase2")]
    {
        let h = unsafe { &*handle };
        h.db.flush_manifest_if_dirty();
        CString::new(h.db.head()).unwrap_or_default().into_raw()
    }
}

#[no_mangle]
pub extern "C" fn nedb_free_str(s: *mut c_char) {
    if !s.is_null() { unsafe { drop(CString::from_raw(s)) } }
}

// ─────────────────────────────────────────────────────────────────────────────
// C API — chain integrity verification (NEDB native tamper-evidence)
// ─────────────────────────────────────────────────────────────────────────────

/// Verify the tamper-evidence of every object in the store.
///
/// This is NEDB's native integrity proof: it walks the content-addressed
/// objects and confirms each one's stored bytes still hash to its address
/// (BLAKE2b). It is the storage-layer equivalent of replaying and re-hashing
/// the whole chain — but native and near-instant: no block deserialization,
/// no script re-execution, no LevelDB-style full reindex.
///
/// The ITC node calls this at startup (NEDB Proof-of-Prefix warm boot) to
/// confirm Node B's local chain is intact before trusting its persisted tip.
/// If the local chain is corrupt the node falls back to a full resync from
/// height 0.
///
/// We deliberately do NOT expose NEDB's NQL query language across this FFI:
/// the FFI is the consensus storage seam and stays a small set of typed
/// primitives. Rich querying lives at the nedbd HTTP layer (Studio / explorer).
///
/// Returns:
///   0   — intact: every object verified, zero problems.
///  >0   — the number of objects that failed verification (tampered/corrupt).
///  -1   — error (null handle).
#[no_mangle]
pub extern "C" fn nedb_verify(handle: *mut NedbHandle) -> c_int {
    if handle.is_null() { return -1; }

    #[cfg(not(feature = "phase2"))]
    {
        // Phase 1 is an in-process BTreeMap with no persisted, content-addressed
        // objects to re-hash — it is trivially intact while the process is live.
        let _ = handle;
        0
    }

    #[cfg(feature = "phase2")]
    {
        let h = unsafe { &*handle };
        let (_checked, problems) = h.db.verify();
        // 0 = intact; otherwise the count of objects that failed verification.
        problems.len().min(i32::MAX as usize) as c_int
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ─────────────────────────────────────────────────────────────────────────────
// C API — bulk scan  (replaces iterator for startup block-index loading)
// ─────────────────────────────────────────────────────────────────────────────

/// Callback signature for nedb_scan.
pub type NedbScanFn = unsafe extern "C" fn(
    key:      *const c_uchar, key_len: usize,
    val:      *const c_uchar, val_len: usize,
    progress: u64,
    total:    u64,
    ctx:      *mut std::ffi::c_void,
);

/// Scan every entry in the database, invoking `callback` for each one.
///
/// Delivers progress = 1..=N and total = N so callers can log or display
/// a progress bar without waiting for the entire load to complete first.
///
/// Returns the total number of entries scanned, or 0 on error.
#[no_mangle]
pub extern "C" fn nedb_scan(
    handle:   *mut NedbHandle,
    callback: NedbScanFn,
    ctx:      *mut std::ffi::c_void,
) -> u64 {
    if handle.is_null() { return 0; }

    #[cfg(not(feature = "phase2"))]
    {
        let inner   = unsafe { &*handle }.inner.lock().unwrap();
        let total   = inner.store.len() as u64;
        for (progress, (k, v)) in inner.store.iter().enumerate() {
            unsafe {
                callback(
                    k.as_ptr(), k.len(),
                    v.as_ptr(), v.len(),
                    (progress + 1) as u64, total,
                    ctx,
                );
            }
        }
        total
    }

    #[cfg(feature = "phase2")]
    {
        // Enumerate through the ENGINE (dual-read: loose objects AND v3 segment
        // packs), exactly like nedb_iter_new — NOT a raw loose-object file walk.
        //
        // The previous implementation walked indexes/<coll>/id/<shard>/* with
        // fs::read_dir and read TWO files per entry (the id pointer + the object).
        // On a v3 segment/pack store that is:
        //   1. Pathologically slow — one CreateFile per object, single-threaded,
        //      ~600k+ syscalls, murderous under Windows real-time AV (this is the
        //      0.1%-CPU "hang" in LoadBlockIndexGuts: it was grinding loose files).
        //   2. INCORRECT — any object the v3 compactor folded into a segment pack
        //      has no loose objects/<2>/<rest> file, so fs::read failed and the
        //      entry was silently skipped (`continue`), yielding an INCOMPLETE
        //      block index. db.list() goes through the engine, which reads loose
        //      objects AND segment packs, so every entry is returned.
        //
        // db.list() loads the collection's nodes up front (one bulk, segment-aware
        // read) — a transient memory cost at boot, but correct and orders of
        // magnitude fewer syscalls than the per-object walk. The C++ callback still
        // streams: it deserializes each (k, v) and filters non-block-index keys.
        let h = unsafe { &*handle };
        let nodes = h.db.list(&h.coll);
        let total = nodes.len() as u64;
        if total == 0 { return 0; }
        let mut progress: u64 = 0;
        for n in nodes {
            let k = match hex::decode(&n.id) { Ok(b) => b, Err(_) => continue };
            let v = match hex::decode(n.data["v"].as_str().unwrap_or("")) {
                Ok(b) => b, Err(_) => continue
            };
            progress += 1;
            unsafe {
                callback(k.as_ptr(), k.len(), v.as_ptr(), v.len(), progress, total, ctx);
            }
        }
        progress
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// C API — iterator
// ─────────────────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn nedb_iter_new(handle: *mut NedbHandle) -> *mut NedbIter {
    if handle.is_null() { return std::ptr::null_mut(); }

    #[cfg(not(feature = "phase2"))]
    {
        let inner = unsafe { &*handle }.inner.lock().unwrap();
        let entries: Vec<(Vec<u8>, Vec<u8>)> = inner.store.iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Box::into_raw(Box::new(NedbIter { entries, pos: usize::MAX }))
    }

    #[cfg(feature = "phase2")]
    {
        let h = unsafe { &*handle };
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = h.db.list(&h.coll)
            .into_iter()
            .filter_map(|n| {
                let k = hex::decode(&n.id).ok()?;
                let v = hex::decode(n.data["v"].as_str().unwrap_or("")).ok()?;
                Some((k, v))
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Box::into_raw(Box::new(NedbIter { entries, pos: usize::MAX }))
    }
}

#[no_mangle]
pub extern "C" fn nedb_iter_free(iter: *mut NedbIter) {
    if !iter.is_null() { unsafe { drop(Box::from_raw(iter)) } }
}

#[no_mangle]
pub extern "C" fn nedb_iter_seek_to_first(iter: *mut NedbIter) {
    if !iter.is_null() { unsafe { (*iter).pos = 0; } }
}

#[no_mangle]
pub extern "C" fn nedb_iter_seek(iter: *mut NedbIter, key: *const c_uchar, key_len: usize) -> c_int {
    if iter.is_null() || key.is_null() { return 0; }
    let it        = unsafe { &mut *iter };
    let key_bytes = unsafe { std::slice::from_raw_parts(key, key_len) };
    it.pos        = it.entries.partition_point(|(k, _)| k.as_slice() < key_bytes);
    if it.pos < it.entries.len() { 1 } else { 0 }
}

#[no_mangle]
pub extern "C" fn nedb_iter_next(iter: *mut NedbIter) {
    if iter.is_null() { return; }
    let it = unsafe { &mut *iter };
    if it.pos != usize::MAX { it.pos += 1; }
}

#[no_mangle]
pub extern "C" fn nedb_iter_valid(iter: *const NedbIter) -> c_int {
    if iter.is_null() { return 0; }
    let it = unsafe { &*iter };
    if it.pos != usize::MAX && it.pos < it.entries.len() { 1 } else { 0 }
}

#[no_mangle]
pub extern "C" fn nedb_iter_key(iter: *const NedbIter, key_out: *mut *mut c_uchar, key_len_out: *mut usize) -> c_int {
    if iter.is_null() { return -1; }
    let it = unsafe { &*iter };
    if it.pos >= it.entries.len() { return -1; }
    let mut boxed: Box<[u8]> = it.entries[it.pos].0.clone().into_boxed_slice();
    unsafe { *key_len_out = boxed.len(); *key_out = boxed.as_mut_ptr(); std::mem::forget(boxed); }
    0
}

#[no_mangle]
pub extern "C" fn nedb_iter_value(iter: *const NedbIter, value_out: *mut *mut c_uchar, value_len_out: *mut usize) -> c_int {
    if iter.is_null() { return -1; }
    let it = unsafe { &*iter };
    if it.pos >= it.entries.len() { return -1; }
    let mut boxed: Box<[u8]> = it.entries[it.pos].1.clone().into_boxed_slice();
    unsafe { *value_len_out = boxed.len(); *value_out = boxed.as_mut_ptr(); std::mem::forget(boxed); }
    0
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests — validate the public C API (work for both phases)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn open(name: &str) -> *mut NedbHandle {
        // Phase 2: use temp dir so Db::open has a real writable path
        #[cfg(feature = "phase2")]
        let path = {
            let mut p = std::env::temp_dir();
            p.push(format!("itcd_test_{}", name));
            CString::new(p.to_string_lossy().as_ref()).unwrap()
        };
        #[cfg(not(feature = "phase2"))]
        let path = CString::new(name).unwrap();
        let h = nedb_open(path.as_ptr(), std::ptr::null());
        assert!(!h.is_null(), "nedb_open returned null for '{name}'");
        h
    }

    fn cleanup(name: &str) {
        #[cfg(feature = "phase2")]
        {
            let mut p = std::env::temp_dir();
            p.push(format!("itcd_test_{}", name));
            let _ = std::fs::remove_dir_all(&p);
        }
        #[cfg(not(feature = "phase2"))]
        let _ = name;
    }

    #[test]
    fn test_open_close() {
        let h = open("t_open");
        nedb_close(h);
        cleanup("t_open");
    }

    #[test]
    fn test_put_get_roundtrip() {
        let h = open("t_rtrip");
        let k: &[u8] = b"block:0";
        let v: &[u8] = b"genesis_payload";
        assert_eq!(nedb_put(h, k.as_ptr(), k.len(), v.as_ptr(), v.len()), 0);
        let mut out: *mut u8 = std::ptr::null_mut();
        let mut olen: usize  = 0;
        assert_eq!(nedb_get(h, k.as_ptr(), k.len(), &mut out, &mut olen), 0);
        assert_eq!(olen, v.len());
        assert_eq!(unsafe { std::slice::from_raw_parts(out, olen) }, v);
        nedb_free_value(out, olen);
        nedb_close(h);
        cleanup("t_rtrip");
    }

    #[test]
    fn test_get_not_found() {
        let h = open("t_miss");
        let k = b"ghost";
        let mut out: *mut u8 = std::ptr::null_mut();
        let mut olen: usize  = 0;
        assert_eq!(nedb_get(h, k.as_ptr(), k.len(), &mut out, &mut olen), 1);
        nedb_close(h);
        cleanup("t_miss");
    }

    #[test]
    fn test_exists_and_del() {
        let h = open("t_exists");
        let k = b"key"; let v = b"val";
        assert_eq!(nedb_exists(h, k.as_ptr(), k.len()), 0);
        nedb_put(h, k.as_ptr(), k.len(), v.as_ptr(), v.len());
        assert_eq!(nedb_exists(h, k.as_ptr(), k.len()), 1);
        nedb_del(h, k.as_ptr(), k.len());
        assert_eq!(nedb_exists(h, k.as_ptr(), k.len()), 0);
        nedb_close(h);
        cleanup("t_exists");
    }

    #[test]
    fn test_is_empty() {
        let h = open("t_empty");
        assert_eq!(nedb_is_empty(h), 1);
        let k = b"k"; let v = b"v";
        nedb_put(h, k.as_ptr(), k.len(), v.as_ptr(), v.len());
        assert_eq!(nedb_is_empty(h), 0);
        nedb_close(h);
        cleanup("t_empty");
    }

    #[test]
    fn test_head_advances() {
        let h   = open("t_head");
        let h0  = { let r = nedb_head(h); let s = unsafe { CStr::from_ptr(r).to_string_lossy().to_string() }; nedb_free_str(r); s };
        let k   = b"block:1"; let v = b"data";
        nedb_put(h, k.as_ptr(), k.len(), v.as_ptr(), v.len());
        let h1  = { let r = nedb_head(h); let s = unsafe { CStr::from_ptr(r).to_string_lossy().to_string() }; nedb_free_str(r); s };
        assert_ne!(h0, h1, "state root must advance after write");
        nedb_close(h);
        cleanup("t_head");
    }

    #[test]
    fn test_batch_write() {
        let h = open("t_batch");
        let c = b"c"; let cv = b"del_me";
        nedb_put(h, c.as_ptr(), c.len(), cv.as_ptr(), cv.len());
        let ops = vec![
            NedbOp { key: b"a".as_ptr(), key_len: 1, value: b"1".as_ptr(), value_len: 1 },
            NedbOp { key: b"b".as_ptr(), key_len: 1, value: b"2".as_ptr(), value_len: 1 },
            NedbOp { key: c.as_ptr(),    key_len: 1, value: std::ptr::null(), value_len: 0 },
        ];
        assert_eq!(nedb_batch_write(h, ops.as_ptr(), ops.len()), 0);
        assert_eq!(nedb_exists(h, b"a".as_ptr(), 1), 1);
        assert_eq!(nedb_exists(h, b"b".as_ptr(), 1), 1);
        assert_eq!(nedb_exists(h, b"c".as_ptr(), 1), 0, "batch delete must remove c");
        nedb_close(h);
        cleanup("t_batch");
    }

    #[test]
    fn test_iterator_ordered() {
        let h = open("t_iter");
        nedb_put(h, b"c".as_ptr(), 1, b"3".as_ptr(), 1);
        nedb_put(h, b"a".as_ptr(), 1, b"1".as_ptr(), 1);
        nedb_put(h, b"b".as_ptr(), 1, b"2".as_ptr(), 1);
        let iter = nedb_iter_new(h);
        nedb_iter_seek_to_first(iter);
        let mut keys: Vec<u8> = Vec::new();
        while nedb_iter_valid(iter) == 1 {
            let mut kp: *mut u8 = std::ptr::null_mut();
            let mut kl: usize   = 0;
            nedb_iter_key(iter, &mut kp, &mut kl);
            keys.push(unsafe { *kp });
            nedb_free_value(kp, kl);
            nedb_iter_next(iter);
        }
        assert_eq!(keys, vec![b'a', b'b', b'c'], "must iterate in ascending order");
        nedb_iter_free(iter);
        nedb_close(h);
        cleanup("t_iter");
    }

    /// NEDB Proof-of-Prefix step 1: a freshly written, untampered store must
    /// verify clean (0 problems). This is the instant local-integrity gate the
    /// ITC node runs at warm boot before trusting its persisted tip.
    #[test]
    fn test_verify_intact_store_reports_no_problems() {
        let h = open("t_verify");
        let k: &[u8] = b"block:0";
        let v: &[u8] = b"genesis_payload";
        nedb_put(h, k.as_ptr(), k.len(), v.as_ptr(), v.len());
        let k2: &[u8] = b"block:1";
        let v2: &[u8] = b"block_one_payload";
        nedb_put(h, k2.as_ptr(), k2.len(), v2.as_ptr(), v2.len());
        assert_eq!(nedb_verify(h), 0, "intact store must report zero integrity problems");
        nedb_close(h);
        cleanup("t_verify");
    }

    /// A null handle is an error, not a silent "intact" — callers must be able
    /// to distinguish "verified clean" (0) from "could not verify" (-1).
    #[test]
    fn test_verify_null_handle_is_error() {
        assert_eq!(nedb_verify(std::ptr::null_mut()), -1);
    }

    /// Phase 1 consensus property: two BTreeMap instances with identical writes
    /// must arrive at identical BLAKE2b chain heads.
    /// Phase 2 (nedb_core_v2): object hashes include timestamps, so parallel
    /// instances diverge — use verify() for tamper-evidence instead.
    #[cfg(not(feature = "phase2"))]
    #[test]
    fn test_head_determinism_is_the_consensus_property() {
        let h1 = open("t_det_node_1");
        let h2 = open("t_det_node_2");
        let writes: &[(&[u8], &[u8])] = &[
            (b"block:0",    b"genesis_bytes"),
            (b"block:1",    b"block_one_bytes"),
            (b"utxo:abc:0", b"satoshi_output"),
        ];
        for &(k, v) in writes {
            nedb_put(h1, k.as_ptr(), k.len(), v.as_ptr(), v.len());
            nedb_put(h2, k.as_ptr(), k.len(), v.as_ptr(), v.len());
        }
        let head1 = { let r = nedb_head(h1); let s = unsafe { CStr::from_ptr(r).to_string_lossy().to_string() }; nedb_free_str(r); s };
        let head2 = { let r = nedb_head(h2); let s = unsafe { CStr::from_ptr(r).to_string_lossy().to_string() }; nedb_free_str(r); s };
        assert_eq!(head1, head2, "CONSENSUS FAILURE: identical writes produced different state roots");
        nedb_close(h1);
        nedb_close(h2);
        cleanup("t_det_node_1");
        cleanup("t_det_node_2");
    }

    /// Phase 2 persistence: data survives close/reopen (real disk storage).
    /// This is the key property that BTreeMap Phase 1 cannot provide.
    #[cfg(feature = "phase2")]
    #[test]
    fn test_phase2_data_persists_across_reopen() {
        let h1 = open("t_persist");
        let k: &[u8] = b"block:genesis";
        let v: &[u8] = b"genesis_data_that_must_survive_restart";
        nedb_put(h1, k.as_ptr(), k.len(), v.as_ptr(), v.len());
        let head_before = { let r = nedb_head(h1); let s = unsafe { CStr::from_ptr(r).to_string_lossy().to_string() }; nedb_free_str(r); s };
        nedb_close(h1);  // flush and close

        // Reopen the same database
        let h2 = open("t_persist");
        let mut out: *mut u8 = std::ptr::null_mut();
        let mut olen: usize  = 0;
        assert_eq!(nedb_get(h2, k.as_ptr(), k.len(), &mut out, &mut olen), 0,
            "data must survive reopen");
        assert_eq!(unsafe { std::slice::from_raw_parts(out, olen) }, v,
            "value must be unchanged after reopen");
        nedb_free_value(out, olen);

        // Head must also be stable (same database state)
        let head_after = { let r = nedb_head(h2); let s = unsafe { CStr::from_ptr(r).to_string_lossy().to_string() }; nedb_free_str(r); s };
        assert_eq!(head_before, head_after,
            "MANIFEST head must be identical on reopen of same unmodified database");

        nedb_close(h2);
        cleanup("t_persist");
    }

    // End-to-end check of the --dagv3 FFI wiring: nedb_set_dag_v3(1) must make a
    // subsequently-opened DB use the v3 segment/pack substrate (the engine then
    // creates objects/segments/). #[ignore]d because it mutates the process-
    // global NEDB_DAG_V3 switch — if it ran in the default parallel suite a
    // sibling test could open a DB inside the v3 window and then reopen it after
    // the switch flips back, breaking its persistence assertion. Run it serially:
    //   cargo test -p nedb-ffi -- --ignored --test-threads=1
    // (v3's storage behavior itself is covered by the engine suite, nedb-engine v2.3.33.)
    #[cfg(feature = "phase2")]
    #[test]
    #[ignore = "mutates process-global NEDB_DAG_V3; run with --ignored --test-threads=1"]
    fn dagv3_switch_opens_segment_store() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("itcd_dagv3_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        nedb_set_dag_v3(1);
        let cpath = CString::new(dir.to_string_lossy().as_ref()).unwrap();
        let h = nedb_open(cpath.as_ptr(), std::ptr::null());
        assert!(!h.is_null(), "open failed under --dagv3");

        let k: &[u8] = b"utxo:0";
        let v: &[u8] = b"coin";
        let op = NedbOp { key: k.as_ptr(), key_len: k.len(), value: v.as_ptr(), value_len: v.len() };
        assert_eq!(nedb_batch_write(h, &op, 1), 0, "batch write failed under v3");
        nedb_close(h); // flush_all() -> segment fsync

        assert!(dir.join("objects").join("segments").is_dir(),
                "v3 must create objects/segments/ when NEDB_DAG_V3 is set before open");

        nedb_set_dag_v3(0); // restore default so nothing else observes v3
        let _ = std::fs::remove_dir_all(&dir);
    }
}
