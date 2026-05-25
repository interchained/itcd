// Copyright (c) 2009-2010 Satoshi Nakamoto
// Copyright (c) 2009-2018 The Interchained Core developers
// Distributed under the MIT software license, see the accompanying
// file COPYING or http://www.opensource.org/licenses/mit-license.php.

#ifndef BITCOIN_POW_H
#define BITCOIN_POW_H

#include <consensus/params.h>

#include <stdint.h>

class CBlockHeader;
class CBlockIndex;
class uint256;

unsigned int GetNextWorkRequired(const CBlockIndex* pindexLast, const CBlockHeader *pblock, const Consensus::Params&);
unsigned int CalculateNextWorkRequired(const CBlockIndex* pindexLast, int64_t nFirstBlockTime, const Consensus::Params&);

/** Check whether a block hash satisfies the proof-of-work requirement specified by nBits */
/**
 * Check proof of work for a block.
 *
 * @param prevBlockTime  Timestamp (seconds) of the previous block. Used
 *                       post-reactivation to decide whether the Yespower
 *                       emergency fallback is armed for this block.
 *                       Pass -1 when unknown (reload/reindex/test paths);
 *                       in that case the fallback is permissively armed
 *                       so already-validated blocks reload cleanly.
 */
bool CheckProofOfWork(uint256 hash, const CBlockHeader& blockHeader, unsigned int nBits, const Consensus::Params& params, int nHeight, int64_t prevBlockTime = -1);
bool CheckProofOfWorkWithHeight(uint256 hash, CBlockHeader block, unsigned int nBits, const Consensus::Params&, int nHeight, int64_t prevBlockTime = -1);

#endif // BITCOIN_POW_H
