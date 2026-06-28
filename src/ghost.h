// Copyright (c) 2026 The Interchained Core developers
// Distributed under the MIT software license, see the accompanying
// file COPYING or http://www.opensource.org/licenses/mit-license.php.
//
// Ghost Protocol — restricted fast-boot readiness model + block-index access seam.
//
// THE INVARIANT (Oracle): a ghost node may know its TIP before it knows its whole
// BODY, but NO subsystem may act as though the whole body exists until the required
// range has been hydrated or synchronously loaded. The crash class
//     (partial index) + (Bitcoin-Core "index is complete" assumption)
//                     + (unchecked GetAncestor / mapBlockIndex walk)
// dies when raw access is replaced by the seam declared below. Build the gate
// first; then the magic.
//
// Trust boundary (LOCKED):
//   NEDB verify()          = local integrity only (stored bytes match their hashes;
//                            untampered since written). NOT consensus validity.
//   Anchor / Proof-of-Prefix / epoch checkpoints = canonical placement.
//   Consensus validation   = full validity (PoW, scripts, UTXO transitions).
//   => Ghost v1 is ANCHOR-ASSISTED, not trustless instant sync. Stated honestly.
//
// UTXO model (LOCKED):
//   global UTXO  = validation / consensus state.
//   wallet UTXO  = a spendability cache for a specific wallet (NOT consensus).
//   Ghost may defer the global UTXO at boot, but full-node serving/validation
//   unlocks only once the global chainstate is ready (GhostState::ChainstateReady+).

#ifndef BITCOIN_GHOST_H
#define BITCOIN_GHOST_H

#include <atomic>

class CBlockIndex;
class uint256;

//! Default for -ghost-protocol. Off = ordinary node: the readiness state machine
//! and the access seam below are inert (every seam call is a pure pass-through to
//! the legacy lookups). On = enter RESTRICTED ghost mode at boot and run the
//! readiness state machine, logging each [GHOST] transition.
static constexpr bool DEFAULT_GHOST_PROTOCOL = false;

//! Ghost readiness states. Forward-only (except Failed). A subsystem gates on the
//! MINIMUM state it needs — never on chainActive.Tip() alone (that is the trap).
enum class GhostState : int {
    Off = 0,              //!< -ghost-protocol not set; behave as a normal node.
    Booting,              //!< entered restricted mode; nothing loaded yet.
    TipLoaded,            //!< persisted tip read (height + hash known).
    NedbVerified,         //!< NEDB content-addressed integrity passed (untampered).
    AnchorPending,        //!< contacting the anchor for Proof-of-Prefix.
    Anchored,             //!< anchor confirmed our tip is on the canonical chain.
    IndexHydrating,       //!< background block-index hydrate in progress.
    IndexReady,           //!< full block index linked + skip-pointered.
    ChainstateHydrating,  //!< global UTXO rebuild in progress.
    ChainstateReady,      //!< global UTXO ready (full validation possible).
    FullReady,            //!< normal full-node behavior unlocked.
    Failed,               //!< unrecoverable; see the [GHOST] log.
};

//! What a caller wants the seam to do when the requested state is NOT yet ready.
//! Centralized by subsystem — never inline this decision at a call site.
enum class GhostAccessPolicy {
    BlockUntilReady,   //!< wait until hydrated (validation / consensus-critical).
    OnDemandHydrate,   //!< synchronously load just the needed range, then proceed.
    QueueForLater,     //!< defer the op and report it pending (wallet).
    ReturnNotReady,    //!< return a not-ready status now (RPC / P2P).
    FailHard,          //!< abort — only for genuinely impossible states.
};

//! Outcome of a seam access.
enum class GhostAccessStatus { Ok, NotReady, Hydrating, OutOfRange, Failed };

//! Result of a gated block-index lookup/ancestor walk. `pindex` is valid iff Ok.
struct BlockIndexResult {
    CBlockIndex*      pindex = nullptr;
    GhostAccessStatus status = GhostAccessStatus::NotReady;
    explicit operator bool() const {
        return status == GhostAccessStatus::Ok && pindex != nullptr;
    }
};

//! Result of a hydrate request.
struct HydrateResult {
    GhostAccessStatus status = GhostAccessStatus::NotReady;
    int hydrated_through = -1;   //!< highest contiguously-hydrated height (-1 = none)
    explicit operator bool() const { return status == GhostAccessStatus::Ok; }
};

//! Process-wide ghost readiness — the single source of truth. Atomic + lock-light
//! so any thread (RPC, wallet, msghand, validation) can query it cheaply.
class GhostReadiness {
public:
    static GhostReadiness& Get();

    bool        Enabled() const { return m_enabled.load(std::memory_order_acquire); }
    void        SetEnabled(bool on) { m_enabled.store(on, std::memory_order_release); }
    GhostState  State() const { return m_state.load(std::memory_order_acquire); }
    void        Advance(GhostState to);   //!< forward-only; logs the [GHOST] transition
    void        Fail(const char* why);

    //! Restricted = ghost enabled and not yet FullReady. Full-node serving and
    //! global validation stay gated while restricted.
    bool Restricted() const { return Enabled() && State() != GhostState::FullReady; }

    //! Highest contiguously-hydrated block height (-1 = none yet).
    int  HydratedThrough() const { return m_hydrated_through.load(std::memory_order_acquire); }
    void SetHydratedThrough(int h) { m_hydrated_through.store(h, std::memory_order_release); }
    bool IsHeightHydrated(int h) const { return h >= 0 && h <= HydratedThrough(); }

private:
    std::atomic<bool>       m_enabled{false};
    std::atomic<GhostState> m_state{GhostState::Off};
    std::atomic<int>        m_hydrated_through{-1};
};

// ── The access seam ──────────────────────────────────────────────────────────
// EVERY historical block-index / ancestor / by-height access that can run during
// ghost boot MUST go through these instead of raw mapBlockIndex[] / GetAncestor()
// / chainActive[]. When ghost is OFF (or the target is already hydrated), these
// are thin, allocation-free pass-throughs to the legacy lookups.

//! Gated index lookup. Never hands back a half-built (nStatus==0) stub.
BlockIndexResult RequireBlockIndex(const uint256& hash, GhostAccessPolicy policy);

//! Gated ancestor walk. Fast-paths when `height` is within the hydrated range;
//! only engages the policy when the walk would cross below it.
BlockIndexResult RequireAncestor(const CBlockIndex* start, int height, GhostAccessPolicy policy);

//! Ensure the index is hydrated through `height` per `policy` before proceeding.
HydrateResult    RequireHydratedThroughHeight(int height, GhostAccessPolicy policy);

//! Soft lookup: returns nullptr unless the block is present AND ready. No policy,
//! no blocking, no hydrate — for callers that already tolerate a null answer.
CBlockIndex*     LookupBlockIndexGhostAware(const uint256& hash);

#endif // BITCOIN_GHOST_H
