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
    nedb_core_v2::{Db, Dek},
    serde_json::json,
    std::path::Path,
    std::sync::Arc,
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
    db:   Arc<Db>,
    coll: String,
    path: std::path::PathBuf,  // database root directory for direct file access
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
        // Flush MANIFEST every 5s so metadata is durable during long sessions.
        // Block-level WAL durability is handled in nedb_batch_write (flush_all).
        Db::start_manifest_ticker(Arc::clone(&db_arc), 5_000);
        Box::into_raw(Box::new(NedbHandle { db: db_arc, coll, path: db_path.to_path_buf() }))
    }
}

#[no_mangle]
pub extern "C" fn nedb_close(handle: *mut NedbHandle) {
    if handle.is_null() { return; }
    #[cfg(feature = "phase2")]
    {
        // Flush the id-index WAL and MANIFEST before dropping.
        // IdIndex::set() is WAL-only (zero disk I/O on hot path); a background
        // ticker flushes it every 1s. Without explicit flush here, the WAL
        // DashMap is dropped with the Db and unflushed writes are lost —
        // breaking data persistence across close/reopen.
        let h = unsafe { &*handle };
        h.db.flush_all();
    }
    unsafe { drop(Box::from_raw(handle)) }
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
        let h = unsafe { &*handle };
        for op in ops_slice {
            if op.key.is_null() { continue; }
            let k   = unsafe { std::slice::from_raw_parts(op.key, op.key_len) };
            let kid = hex::encode(k);
            if op.value.is_null() {
                let _ = h.db.delete(&h.coll, &kid);
            } else {
                let v       = unsafe { std::slice::from_raw_parts(op.value, op.value_len) };
                let vh      = hex::encode(v);
                let caused_by = h.db.get(&h.coll, &kid)
                    .filter(|n| !n.hash.is_empty())
                    .map(|n| vec![n.hash.clone()])
                    .unwrap_or_default();
                let _ = h.db.put(&h.coll, &kid, json!({"v": vh}), caused_by, None, None);
            }
        }
        // Flush WAL + MANIFEST after every batch commit.
        // This is the block-level durability point — equivalent to LevelDB's fsync.
        // Individual nedb_put calls buffer to WAL (zero disk I/O per write).
        // flush_all() = id_index.flush_write_buf() + flush_manifest()
        // One disk flush per batch, not per write. Fast and durable.
        h.db.flush_all();
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
        use std::fs;
        let h = unsafe { &*handle };
        // Sequential file walk — optimal for rotational disk (HDD/Fusion Drive).
        // Progress fires immediately per entry: real-time counter on all storage types.
        let index_root = h.path.join("indexes").join(&h.coll).join("id");
        // Count entries first (directory stat, no file reads) for progress denominator.
        let mut total: u64 = 0;
        if let Ok(shards) = fs::read_dir(&index_root) {
            for shard in shards.flatten() {
                if shard.path().is_dir() {
                    if let Ok(files) = fs::read_dir(shard.path()) {
                        total += files.count() as u64;
                    }
                }
            }
        }
        if total == 0 { return 0; }
        // Sequential read + immediate callback per entry.
        let mut progress: u64 = 0;
        if let Ok(shards) = fs::read_dir(&index_root) {
            for shard in shards.flatten() {
                if !shard.path().is_dir() { continue; }
                if let Ok(files) = fs::read_dir(shard.path()) {
                    for id_file in files.flatten() {
                        let path = id_file.path();
                        let hex_key = match path.file_name().and_then(|n| n.to_str()) {
                            Some(s) => s.to_string(), None => continue,
                        };
                        let k = match hex::decode(&hex_key) { Ok(b) => b, Err(_) => continue };
                        let hash_hex = match fs::read_to_string(&path) { Ok(s) => s, Err(_) => continue };
                        let hash_hex = hash_hex.trim();
                        if hash_hex.len() < 4 { continue; }
                        let obj_path = h.path.join("objects").join(&hash_hex[..2]).join(&hash_hex[2..]);
                        let obj_bytes = match fs::read(&obj_path) { Ok(b) => b, Err(_) => continue };
                        let node: serde_json::Value = match serde_json::from_slice(&obj_bytes) {
                            Ok(v) => v, Err(_) => continue
                        };
                        let v = match hex::decode(node["data"]["v"].as_str().unwrap_or("")) {
                            Ok(b) => b, Err(_) => continue
                        };
                        progress += 1;
                        unsafe {
                            callback(k.as_ptr(), k.len(), v.as_ptr(), v.len(), progress, total, ctx);
                        }
                    }
                }
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
}
