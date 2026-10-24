//! The diff currency and the composer seam: one source block's writes,
//! whatever their provenance.
//!
//! reth speaks EIP-7928 BALs (with a prestateTracer fallback); the venue
//! speaks a polled cache diff. Both flatten to the one currency,
//! [`mirror_anvil::Write`] — the type the COW filter, the applier and the
//! journal already share. The filter lives here because it is the currency's
//! first consumer; the composer trait is the first of the spine's five
//! divergence seams (diff source, reset signal, clock, production, probes).
use crate::shadow::Shadow;
use crate::source::Fut;
use alloy_primitives::{Address, U256};
use mirror_anvil::Write;
use std::collections::{HashMap, HashSet};

/// Where a block's writes came from: one metric label, one log field.
///
/// `Bal` = `eth_getBlockAccessList`; `Trace` = prestateTracer fallback;
/// `Poll` = the venue's cached diff over touched accounts + declared closure;
/// `None` = the chain is static between re-pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum DiffKind {
    Bal,
    Trace,
    Poll,
    None,
}

impl DiffKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bal => "bal",
            Self::Trace => "trace",
            Self::Poll => "poll",
            Self::None => "none",
        }
    }
}

/// One source block's writes (the per-block `kind` records what the composer
/// actually used: a preferred `Bal` composer falls back to `Trace` per block).
#[derive(Debug, Clone)]
pub struct BlockDiff {
    pub n: u64,
    pub kind: DiffKind,
    pub writes: Vec<Write>,
}

/// The pluggable part: how one source block becomes writes. One implementation
/// per profile (reth: BAL → trace; the venue: the polled cache diff); the
/// spine never knows which.
pub trait DiffComposer: Send + Sync {
    /// The composer's preferred kind (the per-block `BlockDiff::kind` is the
    /// truth for what a given block used).
    fn kind(&self) -> DiffKind;
    fn compose(&self, n: u64) -> Fut<'_, mirror_anvil::Result<BlockDiff>>;
}

/// What the mirror keeps fresh.
///
/// The mainnet profile watches whole accounts (every slot of a watched
/// account arrives through the BAL, so `accounts` is the whole set); the venue
/// cannot enumerate touched slots, so it watches accounts for the account
/// fields and a declared slot closure on top. Whole-account fixture ownership
/// is not here: it belongs to the shadow registry (marks without values).
#[derive(Debug, Clone, Default)]
pub struct WatchSet {
    pub accounts: HashSet<Address>,
    /// Declared slots (venue closure; empty on mainnet).
    pub slots: HashMap<Address, HashSet<U256>>,
}

impl WatchSet {
    pub fn new(accounts: HashSet<Address>) -> Self {
        Self {
            accounts,
            slots: HashMap::new(),
        }
    }

    /// Is this write's key watched? Account fields follow the account; a
    /// storage write is watched when its account is (mainnet: all slots) or
    /// its slot is declared (venue closure).
    pub fn contains(&self, w: &Write) -> bool {
        self.contains_key(&w.key())
    }

    /// The key-level form (the read plan asks keys, not writes).
    pub fn contains_key(&self, key: &mirror_anvil::Key) -> bool {
        match key {
            mirror_anvil::Key::Storage { address, slot } => {
                self.accounts.contains(address)
                    || self
                        .slots
                        .get(address)
                        .is_some_and(|slots| slots.contains(slot))
            }
            other => self.accounts.contains(&other.address()),
        }
    }
}

/// The COW filter's counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FilterStats {
    pub not_watched: usize,
    pub skipped_shadowed: usize,
}

/// Keep only the watch set's writes, skipping devnet-owned keys (COW:
/// account fields count as one unit, whole-account fixtures included).
pub fn filter_watched(
    watch: &WatchSet,
    shadow: &Shadow,
    writes: Vec<Write>,
) -> (Vec<Write>, FilterStats) {
    let mut filtered = Vec::new();
    let mut stats = FilterStats::default();
    for w in writes {
        if !watch.contains(&w) {
            stats.not_watched += 1;
            continue;
        }
        if shadow.is_shadowed(&w) {
            stats.skipped_shadowed += 1;
            continue;
        }
        filtered.push(w);
    }
    (filtered, stats)
}
