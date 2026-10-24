//! `RethComposer: DiffComposer` — the mainnet profile's diff source.
//!
//! The native EIP-7928 BAL first; the prestateTracer diff fallback when a
//! block has none (the spec's ordering makes "last change per key = final
//! value" hold; `flatten_bal` implements exactly that). Both flatteners live
//! in `mirror-anvil` beside the currency they produce.
use crate::rpc::Rpc;
use mirror_devnet::diff::{BlockDiff, DiffComposer, DiffKind};
use mirror_devnet::source::Fut;

pub struct RethComposer {
    reth: Rpc,
}

impl RethComposer {
    pub fn new(reth: Rpc) -> Self {
        Self { reth }
    }
}

impl DiffComposer for RethComposer {
    fn kind(&self) -> DiffKind {
        DiffKind::Bal
    }

    /// The window is a single block on the mainnet: the applier advances one
    /// parent block at a time, and the BAL is per block. `from` is the
    /// applier's previous block (part of the shared seam's contract); the
    /// composition itself only needs `to`.
    fn compose(&self, _from: u64, to: u64) -> Fut<'_, mirror_anvil::Result<BlockDiff>> {
        Box::pin(async move {
            let (writes, kind) = fetch_diffs(&self.reth, to).await?;
            Ok(BlockDiff { n: to, kind, writes })
        })
    }
}

/// The parent's diffs: native BAL first, prestateTracer diff fallback.
async fn fetch_diffs(
    reth: &Rpc,
    n: u64,
) -> mirror_anvil::Result<(Vec<mirror_anvil::Write>, DiffKind)> {
    if let Some(bal) = reth.block_access_list(n).await? {
        return Ok((mirror_anvil::flatten_bal(&bal), DiffKind::Bal));
    }
    let traces = reth.trace_block_prestate(n).await?;
    Ok((mirror_anvil::flatten_trace_block(&traces), DiffKind::Trace))
}
