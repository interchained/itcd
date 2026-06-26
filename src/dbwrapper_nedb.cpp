// Copyright (c) 2012-2019 The Interchained Core developers
// Distributed under the MIT software license, see the accompanying
// file COPYING or http://www.opensource.org/licenses/mit-license.php.
//
// NEDB backend implementation of CDBWrapper / CDBBatch / CDBIterator.
// This file replaces src/dbwrapper.cpp when compiling itcd with NEDB storage.
//
// Architecture:
//   ITC C++ consensus layer (P2P, PoW, ITSL, scripts — unchanged)
//       ↕  CDBWrapper shim (this file)
//   nedb-ffi C API  (nedb-ffi/src/lib.rs — cbindgen bridge)
//       ↕  Rust FFI
//   NEDB DAG engine (Phase 2: nedb_core_v2::Db)
//       ↕  nedbd HTTP / nedbd --dag
//   NEDB Studio (NQL explorer, TRACE provenance, AS OF time-travel)
//
// Phase 1: HashMap/BTreeMap-backed in-process store (proves the seam).
// Phase 2: wire in nedb_core_v2::Db — BLAKE2b chain head, MVCC, causal DAG.
//
// © Interchained LLC × Claude Sonnet 4.6

#include <dbwrapper.h>
#include <logging.h>
#include <util/system.h>

#include <cstdio>
#include <cstring>
#include <stdexcept>
#include <string>
#include <vector>
#include <thread>
#include <chrono>

// ---------------------------------------------------------------------------
// dbwrapper_private
// ---------------------------------------------------------------------------

namespace dbwrapper_private {

// NEDB backend: obfuscation key is always the zero vector (XOR = no-op).
// NEDB uses AES-256-GCM natively (NEDB_TMK env var, Phase 2).
// We return w.obfuscate_key directly — it is always zero-filled on open.
const std::vector<unsigned char>& GetObfuscateKey(const CDBWrapper& w)
{
    return w.obfuscate_key;
}

} // namespace dbwrapper_private

// ---------------------------------------------------------------------------
// CDBWrapper static members
// ---------------------------------------------------------------------------

const std::string  CDBWrapper::OBFUSCATE_KEY_KEY    = "\x0bobfuscate_key";
const unsigned int CDBWrapper::OBFUSCATE_KEY_NUM_BYTES = 8;

// ---------------------------------------------------------------------------
// CDBWrapper lifecycle
// ---------------------------------------------------------------------------

CDBWrapper::CDBWrapper(const fs::path& path, size_t nCacheSize,
                       bool fMemory, bool fWipe, bool obfuscate, bool provenance)
    : pdb(nullptr)
{
    m_name = path.string();

    if (fWipe) {
        LogPrintf("NEDB: wiping data directory %s\n", m_name);
        // -reindex / fReset must give nedb_open() a genuinely empty store (a
        // stale MANIFEST/best-block otherwise trips ConnectBlock's
        //   assert(hashPrevBlock == view.GetBestBlock())
        // on the genesis reconnect). But the NEDB store is hundreds of thousands
        // of tiny content-addressed object files, and a SYNCHRONOUS fs::remove_all
        // blocks startup for minutes (looks hung). So rename the tree aside —
        // instant, same filesystem — and delete it on a detached background
        // thread; nedb_open() recreates a fresh store immediately below.
        try {
            if (fs::exists(path)) {
                fs::path trash = path;
                trash += ".wipe-" + std::to_string(
                    std::chrono::steady_clock::now().time_since_epoch().count());
                fs::rename(path, trash);
                std::thread([trash]{ try { fs::remove_all(trash); } catch (...) {} }).detach();
                LogPrintf("NEDB: store moved aside, deleting in background (%s)\n", trash.string());
            }
        } catch (const std::exception& e) {
            // e.g. cross-filesystem rename — fall back to a synchronous remove.
            LogPrintf("NEDB: wipe rename failed (%s); removing synchronously\n", e.what());
            try { if (fs::exists(path)) fs::remove_all(path); } catch (...) {}
        }
    }

    // Obfuscation is a no-op for NEDB (encryption is at the engine level).
    obfuscate_key = std::vector<unsigned char>(OBFUSCATE_KEY_NUM_BYTES, 0);

    // Phase 2: pass the TMK (from NEDB_TMK env var) as the dek parameter.
    // Phase 1: dek is nullptr (no encryption in the HashMap backend).
    const char* dek = nullptr;
    // const char* dek = getenv("NEDB_TMK");  // uncomment for Phase 2 encryption

    LogPrintf("NEDB: opening database '%s'%s\n",
              m_name,
              fMemory ? " (in-memory)" : "");

    // NEDB v3 opt-in (-dagv3): switch the object substrate to the segment/pack
    // store for this (and every subsequently-opened) NEDB database. MUST run
    // before nedb_open() — the engine picks its substrate at open time. v3 is
    // transparent to the KV / Merkle head / AS OF / causal-provenance contract,
    // and existing v2 loose objects stay readable via dual-read, so this is a
    // non-destructive opt-in. Applies to both the block index and the chainstate.
    if (gArgs.GetBoolArg("-dagv3", false)) {
        nedb_set_dag_v3(1);
        LogPrintf("NEDB: v3 segment/pack object store ENABLED for '%s' (-dagv3)\n", m_name);
    }

    // Fast-fsync opt-in (-dagfastsync): use a plain fsync(2) at v3 durability
    // points instead of the OS full barrier (F_FULLFSYNC on macOS). MUST run
    // before nedb_open(). Much faster flush on macOS (Fusion/SATA); no-op
    // off-macOS. Weaker only against power-loss-to-platter — still crash-safe,
    // and the chainstate re-syncs from peers. Pairs with -dagv3.
    if (gArgs.GetBoolArg("-dagfastsync", false)) {
        nedb_set_fast_fsync(1);
        LogPrintf("NEDB: fast-fsync ENABLED for '%s' (-dagfastsync; plain fsync(2) vs F_FULLFSYNC)\n", m_name);
    }

    pdb = nedb_open(m_name.c_str(), dek);
    if (!pdb) {
        throw dbwrapper_error("NEDB: failed to open database: " + m_name);
    }

    // Lookup-table DBs (the block index) carry their causal lineage in the
    // payload itself (CDiskBlockIndex.hashPrev), so NEDB's per-write caused_by
    // is redundant. Opening them no-provenance skips the read-before-write that
    // dominated the block-index flush. The chainstate always keeps provenance.
    if (!provenance) {
        nedb_set_provenance(pdb, 0);
        LogPrintf("NEDB: '%s' opened with causal provenance DISABLED "
                  "(lookup-table fast path; lineage is intrinsic to payload)\n", m_name);
    }

    LogPrintf("NEDB: opened database '%s'\n", m_name);
}

CDBWrapper::~CDBWrapper()
{
    nedb_close(pdb);
    pdb = nullptr;
}

std::vector<unsigned char> CDBWrapper::CreateObfuscateKey() const
{
    // NEDB backend: always returns zero vector (obfuscation is not used).
    return std::vector<unsigned char>(OBFUSCATE_KEY_NUM_BYTES, 0);
}

// ---------------------------------------------------------------------------
// CDBWrapper write path
// ---------------------------------------------------------------------------

bool CDBWrapper::WriteBatch(CDBBatch& batch, bool /*fSync*/)
{
    if (batch.m_ops.empty()) return true;

    // Build the NedbOp array from the accumulated batch entries.
    // The NedbBatchEntry vectors keep the byte data alive for this call.
    std::vector<NedbOp> ops;
    ops.reserve(batch.m_ops.size());

    for (const auto& entry : batch.m_ops) {
        NedbOp op;
        op.key     = entry.key.data();
        op.key_len = entry.key.size();
        if (entry.is_delete) {
            op.value     = nullptr;
            op.value_len = 0;
        } else {
            op.value     = entry.value.data();
            op.value_len = entry.value.size();
        }
        ops.push_back(op);
    }

    int rc = nedb_batch_write(pdb, ops.data(), ops.size());
    if (rc != 0) {
        throw dbwrapper_error("NEDB: batch write failed for database: " + m_name);
    }

    // fSync is a no-op: NEDB's group-commit Sequencer handles fsync internally.
    // Phase 2: the DAG engine syncs on every commit by design.
    return true;
}

// ---------------------------------------------------------------------------
// CDBWrapper misc
// ---------------------------------------------------------------------------

bool CDBWrapper::IsEmpty()
{
    int rc = nedb_is_empty(pdb);
    if (rc < 0) {
        throw dbwrapper_error("NEDB: is_empty failed for database: " + m_name);
    }
    return rc == 1;
}

// ---------------------------------------------------------------------------
// CDBIterator lifecycle
// ---------------------------------------------------------------------------

CDBIterator::~CDBIterator()
{
    nedb_iter_free(piter);
    piter = nullptr;
}

bool CDBIterator::Valid() const
{
    return nedb_iter_valid(piter) == 1;
}

void CDBIterator::SeekToFirst()
{
    nedb_iter_seek_to_first(piter);
}

void CDBIterator::Next()
{
    nedb_iter_next(piter);
}
