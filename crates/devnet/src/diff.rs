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
/// `Poll` = the venue's window diff over touched accounts + declared closures;
/// `None` = nothing was composed (an empty window).
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

/// The pluggable part: how a source's blocks become writes. One implementation
/// per profile (reth: BAL → trace; the venue: the polled cache diff); the
/// spine never knows which.
///
/// The window is `(from, to]`: a per-block composer (reth's BAL) composes the
/// single block `to` — the applier advances one block at a time, so the
/// window is exactly one block — while a window composer (the venue's poll)
/// composes everything since the last composed block, because its source
/// cannot enumerate a single block's writes and the touched set must cover
/// the whole gap after downtime.
pub trait DiffComposer: Send + Sync {
    /// The composer's preferred kind (the per-block `BlockDiff::kind` is the
    /// truth for what a given block used).
    fn kind(&self) -> DiffKind;
    fn compose(&self, from: u64, to: u64) -> Fut<'_, mirror_anvil::Result<BlockDiff>>;

    /// Post-write live parity: re-read the just-written keys and compare them
    /// against the values the composition expected. `None` when this composer
    /// has no live parity (the per-block BAL composer: the applier's sampled
    /// parity is its check); the poll composer answers `Some((ok, failed))` —
    /// the venue's [`crate::spine::ProbeSchedule::LiveParity`].
    fn verify<'a>(
        &'a self,
        writes: &'a [Write],
    ) -> Fut<'a, mirror_anvil::Result<Option<(usize, usize)>>> {
        let _ = writes;
        Box::pin(async move { Ok(None) })
    }
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

    /// Declare slots for a contract (the venue closure): whenever the account
    /// is touched, its declared slots are watched. The plan is the narrower
    /// set; the watch set's job is the COW filter and the audit.
    pub fn declare_slots(&mut self, contract: Address, slots: impl IntoIterator<Item = U256>) {
        self.slots.entry(contract).or_default().extend(slots);
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
