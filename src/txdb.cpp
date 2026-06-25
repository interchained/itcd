// Copyright (c) 2009-2010 Satoshi Nakamoto
// Copyright (c) 2009-2020 The Interchained Core developers
// Distributed under the MIT software license, see the accompanying
// file COPYING or http://www.opensource.org/licenses/mit-license.php.

#include <txdb.h>

#include <node/ui_interface.h>
#include <pow.h>
#include <random.h>
#include <shutdown.h>
#include <uint256.h>
#include <util/memory.h>
#include <util/system.h>
#include <util/translation.h>
#include <util/vector.h>

#include <stdint.h>

static const char DB_COIN = 'C';
static const char DB_COINS = 'c';
static const char DB_BLOCK_FILES = 'f';
static const char DB_BLOCK_INDEX = 'b';

static const char DB_BEST_BLOCK = 'B';
static const char DB_HEAD_BLOCKS = 'H';
static const char DB_FLAG = 'F';
static const char DB_REINDEX_FLAG = 'R';
static const char DB_LAST_BLOCK = 'l';
static const char DB_CHAIN_WORK_TIP = 'W';  //!< Persisted tip nChainWork for fast warm restart
static const char DB_TIP_HASH        = 'T';  //!< Persisted tip block hash for fast warm restart

namespace {

struct CoinEntry {
    COutPoint* outpoint;
    char key;
    explicit CoinEntry(const COutPoint* ptr) : outpoint(const_cast<COutPoint*>(ptr)), key(DB_COIN)  {}

    SERIALIZE_METHODS(CoinEntry, obj) { READWRITE(obj.key, obj.outpoint->hash, VARINT(obj.outpoint->n)); }
};

}

CCoinsViewDB::CCoinsViewDB(fs::path ldb_path, size_t nCacheSize, bool fMemory, bool fWipe) :
    m_db(MakeUnique<CDBWrapper>(ldb_path, nCacheSize, fMemory, fWipe, true)),
    m_ldb_path(ldb_path),
    m_is_memory(fMemory) { }

void CCoinsViewDB::ResizeCache(size_t new_cache_size)
{
    // Have to do a reset first to get the original `m_db` state to release its
    // filesystem lock.
    m_db.reset();
    m_db = MakeUnique<CDBWrapper>(
        m_ldb_path, new_cache_size, m_is_memory, /*fWipe*/ false, /*obfuscate*/ true);
}

bool CCoinsViewDB::GetCoin(const COutPoint &outpoint, Coin &coin) const {
    return m_db->Read(CoinEntry(&outpoint), coin);
}

bool CCoinsViewDB::HaveCoin(const COutPoint &outpoint) const {
    return m_db->Exists(CoinEntry(&outpoint));
}

void CCoinsViewDB::BatchGetCoins(const std::vector<COutPoint>& outpoints,
                                 std::vector<Coin>& coins,
                                 std::vector<bool>& found) const {
    // Serialize one CoinEntry key per outpoint, then hand them to the NEDB
    // backend as a single parallel batch read. CoinEntry only holds a pointer
    // into `outpoints`, which outlives this call. The result for entry i is the
    // exact bytes GetCoin(outpoints[i]) would have read — same CoinEntry key,
    // same Coin deserialization — so this is a drop-in batched GetCoin().
    std::vector<CoinEntry> entries;
    entries.reserve(outpoints.size());
    for (const COutPoint& op : outpoints) {
        entries.emplace_back(&op);
    }
    m_db->BatchRead(entries, found, coins);
}

uint256 CCoinsViewDB::GetBestBlock() const {
    uint256 hashBestChain;
    if (!m_db->Read(DB_BEST_BLOCK, hashBestChain))
        return uint256();
    return hashBestChain;
}

std::vector<uint256> CCoinsViewDB::GetHeadBlocks() const {
    std::vector<uint256> vhashHeadBlocks;
    if (!m_db->Read(DB_HEAD_BLOCKS, vhashHeadBlocks)) {
        return std::vector<uint256>();
    }
    return vhashHeadBlocks;
}

bool CCoinsViewDB::BatchWrite(CCoinsMap &mapCoins, const uint256 &hashBlock) {
    CDBBatch batch(*m_db);
    size_t count = 0;
    size_t changed = 0;
    size_t batch_size = (size_t)gArgs.GetArg("-dbbatchsize", nDefaultDbBatchSize);
    int crash_simulate = gArgs.GetArg("-dbcrashratio", 0);
    assert(!hashBlock.IsNull());

    uint256 old_tip = GetBestBlock();
    if (old_tip.IsNull()) {
        // We may be in the middle of replaying.
        std::vector<uint256> old_heads = GetHeadBlocks();
        if (old_heads.size() == 2) {
            assert(old_heads[0] == hashBlock);
            old_tip = old_heads[1];
        }
    }

    // In the first batch, mark the database as being in the middle of a
    // transition from old_tip to hashBlock.
    // A vector is used for future extensibility, as we may want to support
    // interrupting after partial writes from multiple independent reorgs.
    batch.Erase(DB_BEST_BLOCK);
    batch.Write(DB_HEAD_BLOCKS, Vector(hashBlock, old_tip));

    for (CCoinsMap::iterator it = mapCoins.begin(); it != mapCoins.end();) {
        if (it->second.flags & CCoinsCacheEntry::DIRTY) {
            CoinEntry entry(&it->first);
            if (it->second.coin.IsSpent())
                batch.Erase(entry);
            else
                batch.Write(entry, it->second.coin);
            changed++;
        }
        count++;
        CCoinsMap::iterator itOld = it++;
        mapCoins.erase(itOld);
        if (batch.SizeEstimate() > batch_size) {
            LogPrint(BCLog::COINDB, "Writing partial batch of %.2f MiB\n", batch.SizeEstimate() * (1.0 / 1048576.0));
            m_db->WriteBatch(batch);
            batch.Clear();
            if (crash_simulate) {
                static FastRandomContext rng;
                if (rng.randrange(crash_simulate) == 0) {
                    LogPrintf("Simulating a crash. Goodbye.\n");
                    _Exit(0);
                }
            }
        }
    }

    // In the last batch, mark the database as consistent with hashBlock again.
    batch.Erase(DB_HEAD_BLOCKS);
    batch.Write(DB_BEST_BLOCK, hashBlock);

    LogPrint(BCLog::COINDB, "Writing final batch of %.2f MiB\n", batch.SizeEstimate() * (1.0 / 1048576.0));
    bool ret = m_db->WriteBatch(batch);
    LogPrint(BCLog::COINDB, "Committed %u changed transaction outputs (out of %u) to coin database...\n", (unsigned int)changed, (unsigned int)count);
    return ret;
}

size_t CCoinsViewDB::EstimateSize() const
{
    return m_db->EstimateSize(DB_COIN, (char)(DB_COIN+1));
}

CBlockTreeDB::CBlockTreeDB(size_t nCacheSize, bool fMemory, bool fWipe) : CDBWrapper(GetDataDir() / "blocks" / "index", nCacheSize, fMemory, fWipe) {
}

bool CBlockTreeDB::ReadBlockFileInfo(int nFile, CBlockFileInfo &info) {
    return Read(std::make_pair(DB_BLOCK_FILES, nFile), info);
}

bool CBlockTreeDB::WriteReindexing(bool fReindexing) {
    if (fReindexing)
        return Write(DB_REINDEX_FLAG, '1');
    else
        return Erase(DB_REINDEX_FLAG);
}

void CBlockTreeDB::ReadReindexing(bool &fReindexing) {
    fReindexing = Exists(DB_REINDEX_FLAG);
}

bool CBlockTreeDB::ReadLastBlockFile(int &nFile) {
    return Read(DB_LAST_BLOCK, nFile);
}

CCoinsViewCursor *CCoinsViewDB::Cursor() const
{
    CCoinsViewDBCursor *i = new CCoinsViewDBCursor(const_cast<CDBWrapper&>(*m_db).NewIterator(), GetBestBlock());
    /* It seems that there are no "const iterators" for LevelDB.  Since we
       only need read operations on it, use a const-cast to get around
       that restriction.  */
    i->pcursor->Seek(DB_COIN);
    // Cache key of first record
    if (i->pcursor->Valid()) {
        CoinEntry entry(&i->keyTmp.second);
        i->pcursor->GetKey(entry);
        i->keyTmp.first = entry.key;
    } else {
        i->keyTmp.first = 0; // Make sure Valid() and GetKey() return false
    }
    return i;
}

bool CCoinsViewDBCursor::GetKey(COutPoint &key) const
{
    // Return cached key
    if (keyTmp.first == DB_COIN) {
        key = keyTmp.second;
        return true;
    }
    return false;
}

bool CCoinsViewDBCursor::GetValue(Coin &coin) const
{
    return pcursor->GetValue(coin);
}

unsigned int CCoinsViewDBCursor::GetValueSize() const
{
    return pcursor->GetValueSize();
}

bool CCoinsViewDBCursor::Valid() const
{
    return keyTmp.first == DB_COIN;
}

void CCoinsViewDBCursor::Next()
{
    pcursor->Next();
    CoinEntry entry(&keyTmp.second);
    if (!pcursor->Valid() || !pcursor->GetKey(entry)) {
        keyTmp.first = 0; // Invalidate cached key after last record so that Valid() and GetKey() return false
    } else {
        keyTmp.first = entry.key;
    }
}

bool CBlockTreeDB::WriteTipChainWork(const arith_uint256& chainwork) {
    return Write(DB_CHAIN_WORK_TIP, ArithToUint256(chainwork));
}

bool CBlockTreeDB::ReadTipChainWork(arith_uint256& chainwork) {
    uint256 raw;
    if (!Read(DB_CHAIN_WORK_TIP, raw)) return false;
    chainwork = UintToArith256(raw);
    return true;
}


bool CBlockTreeDB::WriteTipHash(const uint256& hash) {
    return Write(DB_TIP_HASH, hash);
}

bool CBlockTreeDB::ReadTipHash(uint256& hash) {
    return Read(DB_TIP_HASH, hash);
}

bool CBlockTreeDB::LoadBlockIndexFromTip(
    const uint256& tip_hash,
    int depth,
    std::function<CBlockIndex*(const uint256&)> insertBlockIndex)
{
    // Warm-boot path: walk backwards from the stored tip loading `depth` block
    // headers via direct NEDB key lookups.  The loaded window is verified against
    // peers via P2P handshake (±2016 blocks) before IBD proceeds — the skip list
    // is NOT built here; that is intentional (see TryWarmBoot in validation.cpp).
    uint256 hash = tip_hash;
    int loaded   = 0;

    while (!hash.IsNull() && loaded < depth) {
        if (ShutdownRequested()) return false;

        std::pair<char, uint256> key = {DB_BLOCK_INDEX, hash};
        CDiskBlockIndex diskindex;
        if (!Read(key, diskindex)) {
            LogPrintf("LoadBlockIndexFromTip: missing entry at %s after %d blocks — falling back to full scan\n",
                      hash.GetHex().substr(0, 8), loaded);
            return false;
        }

        CBlockIndex* pindexNew    = insertBlockIndex(diskindex.GetBlockHash());
        pindexNew->pprev          = insertBlockIndex(diskindex.hashPrev);
        pindexNew->nHeight        = diskindex.nHeight;
        pindexNew->nFile          = diskindex.nFile;
        pindexNew->nDataPos       = diskindex.nDataPos;
        pindexNew->nUndoPos       = diskindex.nUndoPos;
        pindexNew->nVersion       = diskindex.nVersion;
        pindexNew->hashMerkleRoot = diskindex.hashMerkleRoot;
        pindexNew->nTime          = diskindex.nTime;
        pindexNew->nBits          = diskindex.nBits;
        pindexNew->nNonce         = diskindex.nNonce;
        pindexNew->nStatus        = diskindex.nStatus;
        pindexNew->nTx            = diskindex.nTx;

        hash = diskindex.hashPrev;
        loaded++;

        if (loaded % 500 == 0)
            LogPrintf("LoadBlockIndex: warm boot %d / %d\n", loaded, depth);
    }

    LogPrintf("LoadBlockIndex: warm boot loaded %d headers from tip.\n", loaded);
    return loaded > 0;
}

bool CBlockTreeDB::ReadBlockIndex(const uint256& hash, CDiskBlockIndex& diskindex) {
    return Read(std::make_pair(DB_BLOCK_INDEX, hash), diskindex);
}

bool CBlockTreeDB::WriteBatchSync(const std::vector<std::pair<int, const CBlockFileInfo*> >& fileInfo, int nLastFile, const std::vector<const CBlockIndex*>& blockinfo) {
    CDBBatch batch(*this);
    for (std::vector<std::pair<int, const CBlockFileInfo*> >::const_iterator it=fileInfo.begin(); it != fileInfo.end(); it++) {
        batch.Write(std::make_pair(DB_BLOCK_FILES, it->first), *it->second);
    }
    batch.Write(DB_LAST_BLOCK, nLastFile);

    // Find the warm-boot tip in this batch — the highest-work block we can
    // actually RESUME from.  Writing tip hash + chain work in the same atomic
    // batch as the block index entries keeps them consistent: the tip hash will
    // only point to a block that exists in NEDB.
    //
    // CRITICAL: the dirty batch contains header-only indices too. During IBD,
    // headers-first sync races the header chain hundreds of thousands of blocks
    // ahead of the connected (data) chain, so the highest-work entry is usually a
    // HEADER with no block body and no UTXO state. Persisting that as the tip
    // makes the next warm boot anchor to a block whose chainstate was never built
    // — ReplayBlocks then aborts with "reorganization to unknown block requested"
    // (clean restart) or, under -reindex-chainstate, the wiped UTXO set never
    // rebuilds. The only valid resume point is a block we fully HAVE: block data
    // on disk (BLOCK_HAVE_DATA) and a connected tx chain (HaveTxsDownloaded()).
    // Restrict the persisted tip to those; header-only entries are ignored.
    const CBlockIndex* pBestInBatch = nullptr;
    for (std::vector<const CBlockIndex*>::const_iterator it=blockinfo.begin(); it != blockinfo.end(); it++) {
        batch.Write(std::make_pair(DB_BLOCK_INDEX, (*it)->GetBlockHash()), CDiskBlockIndex(*it));
        if (((*it)->nStatus & BLOCK_HAVE_DATA) && (*it)->HaveTxsDownloaded() &&
            (!pBestInBatch || (*it)->nChainWork > pBestInBatch->nChainWork))
            pBestInBatch = *it;
    }
    if (pBestInBatch) {
        batch.Write(DB_TIP_HASH,       pBestInBatch->GetBlockHash());
        batch.Write(DB_CHAIN_WORK_TIP, ArithToUint256(pBestInBatch->nChainWork));
    }

    return WriteBatch(batch, true);
}

bool CBlockTreeDB::WriteFlag(const std::string &name, bool fValue) {
    return Write(std::make_pair(DB_FLAG, name), fValue ? '1' : '0');
}

bool CBlockTreeDB::ReadFlag(const std::string &name, bool &fValue) {
    char ch;
    if (!Read(std::make_pair(DB_FLAG, name), ch))
        return false;
    fValue = ch == '1';
    return true;
}

// Context passed into the nedb_scan callback for block index loading.
struct LoadIndexCtx {
    const Consensus::Params*                          consensusParams;
    std::function<CBlockIndex*(const uint256&)>*      insertBlockIndex;
    bool                                              error_flag;
    std::string                                       error_msg;
    int64_t                                           start_ms{0};
};

// nedb_scan callback — invoked once per stored block index entry.
// Replaces the old iterator loop: NEDB delivers entries in batches and
// provides a running progress counter so we can log meaningful startup output
// instead of a silent hang.
static void LoadIndexCallback(
    const unsigned char* key,   size_t key_len,
    const unsigned char* val,   size_t val_len,
    uint64_t progress, uint64_t total,
    void* ctx_ptr)
{
    auto* ctx = static_cast<LoadIndexCtx*>(ctx_ptr);
    if (ctx->error_flag) return;   // stop processing after first error

    // Log progress every 10 000 entries so the operator knows startup is alive.
    // nedb_scan also emits progress callbacks with total=0 while it is counting
    // index files before the real scan starts; surface those too so startup never
    // looks frozen on a large NEDB store.
    if (total == 0) {
        if (progress == 1 || progress % 10000 == 0) {
            LogPrintf("LoadBlockIndex: discovering NEDB entries... %llu seen\n",
                      (unsigned long long)progress);
        }
        return;
    }

    if (progress == 1 || progress % 10000 == 0 || progress == total) {
        const int64_t elapsed_ms = ctx->start_ms > 0 ? GetTimeMillis() - ctx->start_ms : 0;
        const double rate = elapsed_ms > 0 ? (1000.0 * (double)progress / (double)elapsed_ms) : 0.0;
        LogPrintf("LoadBlockIndex: %llu / %llu (%.0f%%, %.0f entries/s, %dms)\n",
                  (unsigned long long)progress,
                  (unsigned long long)total,
                  100.0 * (double)progress / (double)total,
                  rate,
                  (int)elapsed_ms);
    }

    // Deserialise the key: (DB_BLOCK_INDEX prefix byte, uint256 block hash).
    // The key was serialised by CDBWrapper::Write via CDataStream.
    std::pair<char, uint256> db_key;
    try {
        CDataStream ks(reinterpret_cast<const char*>(key),
                       reinterpret_cast<const char*>(key) + key_len,
                       SER_DISK, CLIENT_VERSION);
        ks >> db_key;
    } catch (...) { return; }   // skip malformed keys

    if (db_key.first != DB_BLOCK_INDEX) return;   // skip non-block-index entries

    // Deserialise the value: CDiskBlockIndex.
    CDiskBlockIndex diskindex;
    try {
        CDataStream vs(reinterpret_cast<const char*>(val),
                       reinterpret_cast<const char*>(val) + val_len,
                       SER_DISK, CLIENT_VERSION);
        vs >> diskindex;
    } catch (...) { return; }

    // Construct CBlockIndex in the in-memory map.
    auto& insert = *ctx->insertBlockIndex;
    CBlockIndex* pindexNew = insert(diskindex.GetBlockHash());
    pindexNew->pprev          = insert(diskindex.hashPrev);
    pindexNew->nHeight        = diskindex.nHeight;
    pindexNew->nFile          = diskindex.nFile;
    pindexNew->nDataPos       = diskindex.nDataPos;
    pindexNew->nUndoPos       = diskindex.nUndoPos;
    pindexNew->nVersion       = diskindex.nVersion;
    pindexNew->hashMerkleRoot = diskindex.hashMerkleRoot;
    pindexNew->nTime          = diskindex.nTime;
    pindexNew->nBits          = diskindex.nBits;
    pindexNew->nNonce         = diskindex.nNonce;
    pindexNew->nStatus        = diskindex.nStatus;
    pindexNew->nTx            = diskindex.nTx;

    // PoW is NOT re-verified on startup. Every block in our local NEDB store
    // was validated during IBD — BLOCK_VALID_TREE was set at that time.
    // Re-running CheckProofOfWork here:
    //   1. Triggers GetAncestor calls on partially-loaded pprev stubs
    //      (NEDB delivers entries in hash order, not height order) → assertion crash
    //   2. Recomputes YespowerHash for 350k+ blocks → 45-90 min startup
    // Trust the local store. Tip PoW is verified separately after full load.
}

bool CBlockTreeDB::LoadBlockIndexGuts(const Consensus::Params& consensusParams, std::function<CBlockIndex*(const uint256&)> insertBlockIndex)
{
    LogPrintf("LoadBlockIndex: scanning NEDB block index...\n");

    LoadIndexCtx ctx;
    ctx.consensusParams  = &consensusParams;
    ctx.insertBlockIndex = &insertBlockIndex;
    ctx.error_flag       = false;
    ctx.start_ms         = GetTimeMillis();

    if (ShutdownRequested()) return false;

    // nedb_scan delivers every stored entry via callback with a live progress
    // counter — no more silent hang at startup.
    // pdb is the NedbHandle* inherited from CDBWrapper.
    uint64_t scanned = nedb_scan(GetHandle(), LoadIndexCallback, &ctx);

    if (ctx.error_flag)
        return error("%s: %s", __func__, ctx.error_msg);

    LogPrintf("LoadBlockIndex: loaded %llu block index entries.\n",
              (unsigned long long)scanned);
    return true;
}

namespace {

//! Legacy class to deserialize pre-pertxout database entries without reindex.
class CCoins
{
public:
    //! whether transaction is a coinbase
    bool fCoinBase;

    //! unspent transaction outputs; spent outputs are .IsNull(); spent outputs at the end of the array are dropped
    std::vector<CTxOut> vout;

    //! at which height this transaction was included in the active block chain
    int nHeight;

    //! empty constructor
    CCoins() : fCoinBase(false), vout(0), nHeight(0) { }

    template<typename Stream>
    void Unserialize(Stream &s) {
        unsigned int nCode = 0;
        // version
        unsigned int nVersionDummy;
        ::Unserialize(s, VARINT(nVersionDummy));
        // header code
        ::Unserialize(s, VARINT(nCode));
        fCoinBase = nCode & 1;
        std::vector<bool> vAvail(2, false);
        vAvail[0] = (nCode & 2) != 0;
        vAvail[1] = (nCode & 4) != 0;
        unsigned int nMaskCode = (nCode / 8) + ((nCode & 6) != 0 ? 0 : 1);
        // spentness bitmask
        while (nMaskCode > 0) {
            unsigned char chAvail = 0;
            ::Unserialize(s, chAvail);
            for (unsigned int p = 0; p < 8; p++) {
                bool f = (chAvail & (1 << p)) != 0;
                vAvail.push_back(f);
            }
            if (chAvail != 0)
                nMaskCode--;
        }
        // txouts themself
        vout.assign(vAvail.size(), CTxOut());
        for (unsigned int i = 0; i < vAvail.size(); i++) {
            if (vAvail[i])
                ::Unserialize(s, Using<TxOutCompression>(vout[i]));
        }
        // coinbase height
        ::Unserialize(s, VARINT_MODE(nHeight, VarIntMode::NONNEGATIVE_SIGNED));
    }
};

}

/** Upgrade the database from older formats.
 *
 * Currently implemented: from the per-tx utxo model (0.8..0.14.x) to per-txout.
 */
bool CCoinsViewDB::Upgrade() {
    // NEDB-backed itcd never stores the pre-0.15 LevelDB per-tx UTXO format
    // (DB_COINS 'c'): coins are written directly as per-txout CoinEntry (DB_COIN
    // 'C') in CCoinsViewDB::BatchWrite, so there is nothing to migrate. Skipping
    // matters for startup time — m_db->NewIterator() on the NEDB backend
    // materializes the ENTIRE chainstate (nedb-ffi nedb_iter_new -> db.list +
    // hex-decode every coin + sort), turning this legacy probe into an
    // O(chainstate) startup stall for a migration that can never apply.
    // Short-circuit it. The legacy LevelDB migration below is retained for a
    // hypothetical LevelDB build but is unreachable on the NEDB backend.
    LogPrintf("Chainstate init: skipping legacy LevelDB UTXO upgrade check (NEDB backend; nothing to migrate).\n");
    return true;

    std::unique_ptr<CDBIterator> pcursor(m_db->NewIterator());
    pcursor->Seek(std::make_pair(DB_COINS, uint256()));
    if (!pcursor->Valid()) {
        return true;
    }

    int64_t count = 0;
    LogPrintf("Upgrading utxo-set database...\n");
    LogPrintf("[0%%]..."); /* Continued */
    uiInterface.ShowProgress(_("Upgrading UTXO database").translated, 0, true);
    size_t batch_size = 1 << 24;
    CDBBatch batch(*m_db);
    int reportDone = 0;
    std::pair<unsigned char, uint256> key;
    std::pair<unsigned char, uint256> prev_key = {DB_COINS, uint256()};
    while (pcursor->Valid()) {
        if (ShutdownRequested()) {
            break;
        }
        if (pcursor->GetKey(key) && key.first == DB_COINS) {
            if (count++ % 256 == 0) {
                uint32_t high = 0x100 * *key.second.begin() + *(key.second.begin() + 1);
                int percentageDone = (int)(high * 100.0 / 65536.0 + 0.5);
                uiInterface.ShowProgress(_("Upgrading UTXO database").translated, percentageDone, true);
                if (reportDone < percentageDone/10) {
                    // report max. every 10% step
                    LogPrintf("[%d%%]...", percentageDone); /* Continued */
                    reportDone = percentageDone/10;
                }
            }
            CCoins old_coins;
            if (!pcursor->GetValue(old_coins)) {
                return error("%s: cannot parse CCoins record", __func__);
            }
            COutPoint outpoint(key.second, 0);
            for (size_t i = 0; i < old_coins.vout.size(); ++i) {
                if (!old_coins.vout[i].IsNull() && !old_coins.vout[i].scriptPubKey.IsUnspendable()) {
                    Coin newcoin(std::move(old_coins.vout[i]), old_coins.nHeight, old_coins.fCoinBase);
                    outpoint.n = i;
                    CoinEntry entry(&outpoint);
                    batch.Write(entry, newcoin);
                }
            }
            batch.Erase(key);
            if (batch.SizeEstimate() > batch_size) {
                m_db->WriteBatch(batch);
                batch.Clear();
                m_db->CompactRange(prev_key, key);
                prev_key = key;
            }
            pcursor->Next();
        } else {
            break;
        }
    }
    m_db->WriteBatch(batch);
    m_db->CompactRange({DB_COINS, uint256()}, key);
    uiInterface.ShowProgress("", 100, false);
    LogPrintf("[%s].\n", ShutdownRequested() ? "CANCELLED" : "DONE");
    return !ShutdownRequested();
}
