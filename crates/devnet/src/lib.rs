//! `mirror-devnet` — the devnet plane: shadow registry, journal, state, the
//! admin surface, deal, probes, metrics, and the unshadow re-sync `Source`.
//! The devnet axioms live here; the projects compose these with chain clients.
//! Clocks: `block` = the **devnet chain's height** (T on the mainnet profile,
//! V on the venue profile — the shared crates drive whichever chain the
//! profile owns); `pin` = that chain's fork base.
pub mod admin;
pub mod deal;
pub mod epoch;
pub mod error;
pub mod journal;
pub mod metrics;
pub mod plane;
pub mod probes;
pub mod scan;
pub mod shadow;
pub mod shared;
pub mod source;
pub mod state;
pub mod store;

pub use error::{Error, Result};
pub use shared::Shared;

/// A scratch directory for one test (per test + process: the tests may run
/// concurrently). One definition, so no test forgets the process id and
/// collides with a parallel run.
#[cfg(test)]
pub(crate) fn test_dir(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("mirror-devnet-{tag}-{}", std::process::id()))
}

#[cfg(test)]
mod design_assertions {
    /// The Rust-for-Rustaceans pin (plan §11.3 rule 11).
    fn is_normal<T: Send + Sync + Sized + Unpin>() {}

    #[test]
    fn public_types_are_normal() {
        is_normal::<crate::Shared<crate::state::StateFile>>();
        is_normal::<crate::state::ChainCursor>();
        is_normal::<crate::shadow::Shadow>();
        is_normal::<crate::journal::Journal>();
        is_normal::<crate::journal::JournalEntry>();
        is_normal::<crate::journal::BeforeValue>();
        is_normal::<crate::plane::Plane>();
        is_normal::<crate::admin::AdminSurface>();
        is_normal::<crate::admin::EpochResponse>();
        is_normal::<crate::admin::ShadowOutcome>();
        is_normal::<crate::admin::RewindOutcome>();
        is_normal::<crate::admin::RewindResponse>();
        is_normal::<crate::admin::PauseResponse>();
        is_normal::<crate::admin::ResumeResponse>();
        is_normal::<crate::admin::TimeOutcome>();
        is_normal::<crate::deal::DealOutcome>();
        is_normal::<crate::deal::DealRequest>();
        is_normal::<crate::probes::ProbeOutcome>();
        is_normal::<crate::probes::ProbeRunner<'static>>();
        is_normal::<crate::scan::ScanWalk>();
        is_normal::<crate::epoch::Epoch>();
        is_normal::<crate::Error>();
    }
}
