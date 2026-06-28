// Copyright (c) 2026 The Interchained Core developers
// Distributed under the MIT software license, see the accompanying
// file COPYING or http://www.opensource.org/licenses/mit-license.php.
//
// Ghost Protocol — readiness state machine + block-index access seam (impl).
//
// PR-1 scope: behavior-neutral foundation. When ghost is OFF (the only state
// until PR-2 wires the flag), every seam function is an EXACT pass-through to the
// legacy LookupBlockIndex()/GetAncestor() — a normal node is byte-for-byte
// unchanged. The ghost-active branches are the hooks PR-3+ fills in per subsystem
// (BLOCK_UNTIL_READY / ON_DEMAND_HYDRATE / QUEUE_FOR_LATER / RETURN_NOT_READY).

#include <ghost.h>

#include <chain.h>
#include <logging.h>
#include <uint256.h>
#include <validation.h>   // LookupBlockIndex()

GhostReadiness& GhostReadiness::Get()
{
    static GhostReadiness g_ghost;   // thread-safe init (C++11+)
    return g_ghost;
}

static const char* GhostStateName(GhostState s)
{
    switch (s) {
    case GhostState::Off:                 return "off";
    case GhostState::Booting:             return "booting";
    case GhostState::TipLoaded:           return "tip-loaded";
    case GhostState::NedbVerified:        return "nedb-verified";
    case GhostState::AnchorPending:       return "anchor-pending";
    case GhostState::Anchored:            return "anchored";
    case GhostState::IndexHydrating:      return "index-hydrating";
    case GhostState::IndexReady:          return "index-ready";
    case GhostState::ChainstateHydrating: return "chainstate-hydrating";
    case GhostState::ChainstateReady:     return "chainstate-ready";
    case GhostState::FullReady:           return "full-ready";
    case GhostState::Failed:              return "FAILED";
    }
    return "?";
}

void GhostReadiness::Advance(GhostState to)
{
    // Forward-only. (Failed is set out-of-band via Fail().)
    GhostState from = m_state.load(std::memory_order_acquire);
    if (static_cast<int>(to) <= static_cast<int>(from)) return;
    m_state.store(to, std::memory_order_release);
    LogPrintf("[GHOST] %s -> %s\n", GhostStateName(from), GhostStateName(to));
}

void GhostReadiness::Fail(const char* why)
{
    m_state.store(GhostState::Failed, std::memory_order_release);
    LogPrintf("[GHOST] FAILED: %s\n", why ? why : "(unspecified)");
}

// ── Access seam ──────────────────────────────────────────────────────────────
// nStatus == 0 marks an unloaded stub (default-constructed CBlockIndex). A real,
// loaded entry always has nStatus != 0 (set from disk). The seam never hands back
// a stub during ghost mode — that is the precise crash the gate exists to prevent.

CBlockIndex* LookupBlockIndexGhostAware(const uint256& hash)
{
    CBlockIndex* pindex = LookupBlockIndex(hash);
    if (!GhostReadiness::Get().Enabled()) return pindex;          // normal node: unchanged
    if (pindex && pindex->nStatus == 0) return nullptr;           // ghost: hide unloaded stub
    return pindex;
}

BlockIndexResult RequireBlockIndex(const uint256& hash, GhostAccessPolicy policy)
{
    if (!GhostReadiness::Get().Enabled()) {
        return BlockIndexResult{LookupBlockIndex(hash), GhostAccessStatus::Ok};
    }
    CBlockIndex* pindex = LookupBlockIndex(hash);
    if (pindex && pindex->nStatus != 0) {
        return BlockIndexResult{pindex, GhostAccessStatus::Ok};
    }
    // Not (yet) hydrated. PR-3+ implements the policy (block / on-demand / queue);
    // for now report honestly rather than dereference a stub.
    (void)policy;
    return BlockIndexResult{nullptr, GhostAccessStatus::NotReady};
}

// NOTE: for the OnDemandHydrate / BlockUntilReady policies this walks via
// CBlockIndex::GetAncestor(), which under ghost demand-loads missing parents
// through WarmBootLoadParent() — that path asserts cs_main. Callers using those
// policies MUST hold cs_main (exactly as a raw GetAncestor() call requires).
BlockIndexResult RequireAncestor(const CBlockIndex* start, int height, GhostAccessPolicy policy)
{
    if (!start || height < 0 || height > start->nHeight) {
        return BlockIndexResult{nullptr, GhostAccessStatus::OutOfRange};
    }
    if (!GhostReadiness::Get().Enabled()) {
        return BlockIndexResult{const_cast<CBlockIndex*>(start->GetAncestor(height)),
                                GhostAccessStatus::Ok};
    }
    // Fast path: target is within the hydrated range -> the normal walk is safe.
    if (GhostReadiness::Get().IsHeightHydrated(height)) {
        CBlockIndex* anc = const_cast<CBlockIndex*>(start->GetAncestor(height));
        return BlockIndexResult{anc, anc ? GhostAccessStatus::Ok : GhostAccessStatus::NotReady};
    }
    // Below the hydrated front: honor the caller's policy.
    switch (policy) {
    case GhostAccessPolicy::OnDemandHydrate:
    case GhostAccessPolicy::BlockUntilReady: {
        // GetAncestor() walks via the demand-load chokepoint (WarmBootLoadParent),
        // which synchronously pulls each missing parent from NEDB. Consensus-
        // critical callers must never proceed on a partial view, so we load the
        // ancestor now rather than report not-ready. (BlockUntilReady is satisfied
        // by this synchronous load while the background hydrate front sits below
        // `height`; a later revision may instead wait on the front to advance.)
        CBlockIndex* anc = const_cast<CBlockIndex*>(start->GetAncestor(height));
        return BlockIndexResult{anc, anc ? GhostAccessStatus::Ok : GhostAccessStatus::NotReady};
    }
    case GhostAccessPolicy::QueueForLater:
    case GhostAccessPolicy::ReturnNotReady:
    case GhostAccessPolicy::FailHard:
    default:
        // Non-blocking callers (wallet / RPC / P2P): report not-ready so the caller
        // can defer the op or answer "syncing", instead of forcing a disk load.
        return BlockIndexResult{nullptr, GhostAccessStatus::NotReady};
    }
}

HydrateResult RequireHydratedThroughHeight(int height, GhostAccessPolicy policy)
{
    GhostReadiness& g = GhostReadiness::Get();
    if (!g.Enabled() || g.IsHeightHydrated(height)) {
        return HydrateResult{GhostAccessStatus::Ok, g.HydratedThrough()};
    }
    // PR-3+ : block / on-demand-hydrate / queue / not-ready per policy.
    (void)policy;
    return HydrateResult{GhostAccessStatus::NotReady, g.HydratedThrough()};
}
