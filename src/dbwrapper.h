// Copyright (c) 2012-2019 The Interchained Core developers
// Distributed under the MIT software license, see the accompanying
// file COPYING or http://www.opensource.org/licenses/mit-license.php.
//
// NEDB backend — LevelDB replaced by the NEDB causal DAG engine.
// Public interface (CDBWrapper, CDBBatch, CDBIterator) is unchanged.
// See nedb-ffi/ for the Rust FFI layer and NEDB.md for architecture notes.
//
// Phase 1: in-process BTreeMap (HashMap-backed, ordered) — proves the seam.
// Phase 2: nedb_core_v2::Db — BLAKE2b chain head, MVCC AS OF, causal DAG,
//          AES-256-GCM at-rest encryption, deterministic state roots.

#ifndef BITCOIN_DBWRAPPER_H
#define BITCOIN_DBWRAPPER_H

#include <clientversion.h>
#include <fs.h>
#include <serialize.h>
#include <streams.h>
#include <util/system.h>
#include <util/strencodings.h>

// NEDB C FFI — replaces #include <leveldb/db.h> and <leveldb/write_batch.h>
#include "../nedb-ffi/nedb.h"

#include <cstdint>
#include <stdexcept>
#include <string>
#include <vector>

static const size_t DBWRAPPER_PREALLOC_KEY_SIZE   = 64;
static const size_t DBWRAPPER_PREALLOC_VALUE_SIZE = 1024;

class dbwrapper_error : public std::runtime_error
{
public:
    explicit dbwrapper_error(const std::string& msg) : std::runtime_error(msg) {}
};

class CDBWrapper;

/** Implementation details of the database layer. */
namespace dbwrapper_private {

/** Return the obfuscation key for @w.
 *
 *  NEDB backend: obfuscation is always the zero vector (XOR is a no-op).
 *  Encryption is handled natively by the NEDB DAG engine (AES-256-GCM via TMK).
 *  This function is retained for API compatibility with code that calls
 *  dbwrapper_private::GetObfuscateKey() directly.
 */
const std::vector<unsigned char>& GetObfuscateKey(const CDBWrapper& w);

} // namespace dbwrapper_private

// ---------------------------------------------------------------------------
// CDBBatch — accumulates put / erase operations for atomic commit
// ---------------------------------------------------------------------------

/** An individual pending write.  value is empty + is_delete=true for erases. */
struct NedbBatchEntry {
    std::vector<unsigned char> key;
    std::vector<unsigned char> value;
    bool is_delete;
};

/** Batch of changes queued to be written to a CDBWrapper */
class CDBBatch
{
    friend class CDBWrapper;

private:
    const CDBWrapper& parent;

    /** Accumulated operations — written atomically via nedb_batch_write(). */
    std::vector<NedbBatchEntry> m_ops;

    CDataStream ssKey;
    CDataStream ssValue;

    size_t size_estimate;

public:
    explicit CDBBatch(const CDBWrapper& _parent)
        : parent(_parent),
          ssKey(SER_DISK, CLIENT_VERSION),
          ssValue(SER_DISK, CLIENT_VERSION),
          size_estimate(0) {}

    void Clear()
    {
        m_ops.clear();
        size_estimate = 0;
    }

    template <typename K, typename V>
    void Write(const K& key, const V& value)
    {
        ssKey.reserve(DBWRAPPER_PREALLOC_KEY_SIZE);
        ssKey << key;

        ssValue.reserve(DBWRAPPER_PREALLOC_VALUE_SIZE);
        ssValue << value;
        // NEDB backend: obfuscate_key is all zeros, so XOR is a no-op.
        // NEDB's own AES-256-GCM encryption (Phase 2) supersedes it entirely.
        ssValue.Xor(dbwrapper_private::GetObfuscateKey(parent));

        NedbBatchEntry entry;
        entry.key       = std::vector<unsigned char>(ssKey.begin(),   ssKey.end());
        entry.value     = std::vector<unsigned char>(ssValue.begin(), ssValue.end());
        entry.is_delete = false;

        // Size estimate mirrors the LevelDB wire format (varint + bytes).
        size_estimate += 3
            + (entry.key.size()   > 127) + entry.key.size()
            + (entry.value.size() > 127) + entry.value.size();

        m_ops.push_back(std::move(entry));
        ssKey.clear();
        ssValue.clear();
    }

    template <typename K>
    void Erase(const K& key)
    {
        ssKey.reserve(DBWRAPPER_PREALLOC_KEY_SIZE);
        ssKey << key;

        NedbBatchEntry entry;
        entry.key       = std::vector<unsigned char>(ssKey.begin(), ssKey.end());
        entry.is_delete = true;

        size_estimate += 2 + (entry.key.size() > 127) + entry.key.size();

        m_ops.push_back(std::move(entry));
        ssKey.clear();
    }

    size_t SizeEstimate() const { return size_estimate; }
};

// ---------------------------------------------------------------------------
// CDBIterator — snapshot iterator over a NEDB database
// ---------------------------------------------------------------------------

class CDBIterator
{
private:
    const CDBWrapper& parent;
    NedbIter*         piter;

public:
    CDBIterator(const CDBWrapper& _parent, NedbIter* _piter)
        : parent(_parent), piter(_piter) {}
    ~CDBIterator();

    bool Valid() const;
    void SeekToFirst();

    template <typename K>
    void Seek(const K& key)
    {
        CDataStream ssKey(SER_DISK, CLIENT_VERSION);
        ssKey.reserve(DBWRAPPER_PREALLOC_KEY_SIZE);
        ssKey << key;
        nedb_iter_seek(piter,
                       reinterpret_cast<const unsigned char*>(ssKey.data()),
                       ssKey.size());
    }

    void Next();

    template <typename K>
    bool GetKey(K& key)
    {
        unsigned char* kptr  = nullptr;
        size_t         klen  = 0;
        if (nedb_iter_key(piter, &kptr, &klen) != 0) return false;
        try {
            CDataStream ssKey(reinterpret_cast<char*>(kptr),
                              reinterpret_cast<char*>(kptr) + klen,
                              SER_DISK, CLIENT_VERSION);
            ssKey >> key;
        } catch (const std::exception&) {
            nedb_free_value(kptr, klen);
            return false;
        }
        nedb_free_value(kptr, klen);
        return true;
    }

    template <typename V>
    bool GetValue(V& value)
    {
        unsigned char* vptr = nullptr;
        size_t         vlen = 0;
        if (nedb_iter_value(piter, &vptr, &vlen) != 0) return false;
        try {
            CDataStream ssValue(reinterpret_cast<char*>(vptr),
                                reinterpret_cast<char*>(vptr) + vlen,
                                SER_DISK, CLIENT_VERSION);
            // Obfuscation is a no-op (zero key) — kept for ABI compatibility.
            ssValue.Xor(dbwrapper_private::GetObfuscateKey(parent));
            ssValue >> value;
        } catch (const std::exception&) {
            nedb_free_value(vptr, vlen);
            return false;
        }
        nedb_free_value(vptr, vlen);
        return true;
    }

    unsigned int GetValueSize()
    {
        unsigned char* vptr = nullptr;
        size_t         vlen = 0;
        if (nedb_iter_value(piter, &vptr, &vlen) != 0) return 0;
        nedb_free_value(vptr, vlen);
        return static_cast<unsigned int>(vlen);
    }
};

// ---------------------------------------------------------------------------
// CDBWrapper — NEDB-backed replacement for LevelDB CDBWrapper
// ---------------------------------------------------------------------------

class CDBWrapper
{
    friend const std::vector<unsigned char>&
        dbwrapper_private::GetObfuscateKey(const CDBWrapper& w);

private:
    //! NEDB database handle (replaces leveldb::DB* pdb)
    NedbHandle* pdb;

    //! The name / path of this database instance.
    std::string m_name;

    //! Obfuscation key — always zero for the NEDB backend.
    //! NEDB uses AES-256-GCM (Phase 2, NEDB_TMK env var) instead.
    //! Kept for compatibility with dbwrapper_private::GetObfuscateKey().
    std::vector<unsigned char> obfuscate_key;

    static const std::string  OBFUSCATE_KEY_KEY;
    static const unsigned int OBFUSCATE_KEY_NUM_BYTES;

    std::vector<unsigned char> CreateObfuscateKey() const;


protected:
    //! Expose raw NEDB handle to subclasses (e.g. CBlockTreeDB → nedb_scan).
    NedbHandle* GetHandle() const { return pdb; }

public:
    /**
     * @param path       Filesystem path for the NEDB data directory.
     * @param nCacheSize Ignored (NEDB manages its own memory).
     * @param fMemory    If true, use an in-memory store (no persistence).
     * @param fWipe      If true, wipe existing data before opening.
     * @param obfuscate  Ignored — NEDB uses AES-256-GCM, not XOR obfuscation.
     */
    CDBWrapper(const fs::path& path, size_t nCacheSize,
               bool fMemory = false, bool fWipe = false, bool obfuscate = false);
    ~CDBWrapper();

    CDBWrapper(const CDBWrapper&)            = delete;
    CDBWrapper& operator=(const CDBWrapper&) = delete;

    template <typename K, typename V>
    bool Read(const K& key, V& value) const
    {
        CDataStream ssKey(SER_DISK, CLIENT_VERSION);
        ssKey.reserve(DBWRAPPER_PREALLOC_KEY_SIZE);
        ssKey << key;

        unsigned char* vptr = nullptr;
        size_t         vlen = 0;
        int rc = nedb_get(pdb,
                          reinterpret_cast<const unsigned char*>(ssKey.data()),
                          ssKey.size(),
                          &vptr, &vlen);
        if (rc == 1) return false; // not found
        if (rc != 0) {
            throw dbwrapper_error("NEDB read failure for key in " + m_name);
        }
        try {
            CDataStream ssValue(reinterpret_cast<char*>(vptr),
                                reinterpret_cast<char*>(vptr) + vlen,
                                SER_DISK, CLIENT_VERSION);
            ssValue.Xor(obfuscate_key); // no-op (zero key)
            ssValue >> value;
        } catch (const std::exception&) {
            nedb_free_value(vptr, vlen);
            return false;
        }
        nedb_free_value(vptr, vlen);
        return true;
    }

    /**
     * Batch point-read: fetch many keys in a single FFI call. On the NEDB
     * backend the keys are read in PARALLEL (rayon) — the read-side twin of
     * WriteBatch(). For each i in [0, keys.size()): found_out[i] is set true and
     * values_out[i] holds the deserialized value iff keys[i] is present;
     * otherwise found_out[i] is false and values_out[i] is left default.
     *
     * This is a non-throwing optimization primitive: a whole-batch failure or a
     * per-entry deserialization error is reported as "not found", so the caller
     * can safely fall back to a normal Read() for the missing keys. It must
     * never produce a different result than N individual Read() calls would.
     */
    template <typename K, typename V>
    void BatchRead(const std::vector<K>& keys,
                   std::vector<bool>& found_out,
                   std::vector<V>& values_out) const
    {
        const size_t n = keys.size();
        found_out.assign(n, false);
        values_out.resize(n);
        if (n == 0) return;

        // Serialize every key; keep the byte buffers alive across the FFI call.
        std::vector<std::vector<unsigned char>> key_bufs(n);
        std::vector<NedbOp> ops(n);
        for (size_t i = 0; i < n; ++i) {
            CDataStream ssKey(SER_DISK, CLIENT_VERSION);
            ssKey.reserve(DBWRAPPER_PREALLOC_KEY_SIZE);
            ssKey << keys[i];
            key_bufs[i] = std::vector<unsigned char>(ssKey.begin(), ssKey.end());
            ops[i].key       = key_bufs[i].data();
            ops[i].key_len   = key_bufs[i].size();
            ops[i].value     = nullptr;
            ops[i].value_len = 0;
        }

        std::vector<unsigned char*> vptrs(n, nullptr);
        std::vector<size_t>         vlens(n, 0);
        if (nedb_get_batch(pdb, ops.data(), n, vptrs.data(), vlens.data()) != 0) {
            return; // whole-batch failure: leave everything not-found
        }

        for (size_t i = 0; i < n; ++i) {
            if (vptrs[i] == nullptr) continue; // key absent
            try {
                CDataStream ssValue(reinterpret_cast<char*>(vptrs[i]),
                                    reinterpret_cast<char*>(vptrs[i]) + vlens[i],
                                    SER_DISK, CLIENT_VERSION);
                ssValue.Xor(obfuscate_key); // no-op (zero key)
                ssValue >> values_out[i];
                found_out[i] = true;
            } catch (const std::exception&) {
                found_out[i] = false; // treat as miss; caller re-reads via Read()
            }
            nedb_free_value(vptrs[i], vlens[i]);
        }
    }

    template <typename K, typename V>
    bool Write(const K& key, const V& value, bool fSync = false)
    {
        CDBBatch batch(*this);
        batch.Write(key, value);
        return WriteBatch(batch, fSync);
    }

    template <typename K>
    bool Exists(const K& key) const
    {
        CDataStream ssKey(SER_DISK, CLIENT_VERSION);
        ssKey.reserve(DBWRAPPER_PREALLOC_KEY_SIZE);
        ssKey << key;
        int rc = nedb_exists(pdb,
                             reinterpret_cast<const unsigned char*>(ssKey.data()),
                             ssKey.size());
        if (rc < 0) {
            throw dbwrapper_error("NEDB exists failure for key in " + m_name);
        }
        return rc == 1;
    }

    template <typename K>
    bool Erase(const K& key, bool fSync = false)
    {
        CDBBatch batch(*this);
        batch.Erase(key);
        return WriteBatch(batch, fSync);
    }

    bool WriteBatch(CDBBatch& batch, bool fSync = false);

    /** Returns approximate NEDB memory usage.
     *  Phase 1: returns 0 (exact tracking deferred to Phase 2). */
    size_t DynamicMemoryUsage() const { return 0; }

    CDBIterator* NewIterator()
    {
        return new CDBIterator(*this, nedb_iter_new(pdb));
    }

    /** Return true if the database contains no entries. */
    bool IsEmpty();

    /** Returns an approximate size estimate for the key range [key_begin, key_end).
     *  NEDB backend: returns 0 (size estimates are a LevelDB optimisation hint). */
    template <typename K>
    size_t EstimateSize(const K& /*key_begin*/, const K& /*key_end*/) const
    {
        return 0;
    }

    /** Compact a range of keys.
     *  NEDB backend: no-op — the DAG engine manages its own storage layout. */
    template <typename K>
    void CompactRange(const K& /*key_begin*/, const K& /*key_end*/) const {}

    /** Return the current BLAKE2b chain head (state root) as a hex string.
     *  In Phase 2 this is the deterministic consensus proof: two nodes that
     *  have processed the same chain will produce identical heads. */
    std::string GetStateRoot() const
    {
        char* head = nedb_head(pdb);
        std::string result(head ? head : "");
        nedb_free_str(head);
        return result;
    }

    /** Verify NEDB chain integrity (native BLAKE2b tamper-evidence).
     *
     *  Returns the number of objects that failed verification: 0 means the
     *  store is intact.  A negative value means the verify call itself failed
     *  (e.g. null handle).  This is the instant integrity proof the node runs
     *  at warm boot before trusting the NEDB-persisted tip — the storage
     *  layer's answer to "does Node B's local chain still hold together?".
     *  Native and near-instant: no block deserialization, no LevelDB-style
     *  full reindex. */
    int Verify() const
    {
        return nedb_verify(pdb);
    }
};

#endif // BITCOIN_DBWRAPPER_H
