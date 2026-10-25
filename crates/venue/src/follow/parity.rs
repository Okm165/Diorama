//! The window's measurement: the live parity — the composed window's storage
//! keys re-read from V and compared with what the composition expected. The
//! venue's counterpart of the applier's drift lens (`apply/parity.rs`): the
//! same contract `(sampled, sampled_ok)`, a different mechanism (the applier
//! compares against the parent at the applied block; the follow compares
//! against the target it just landed on — the values the composition expected
//! are the expected side).
use crate::ctx::Ctx;
use crate::error::Result;
use mirror_devnet::spine::ProbeSchedule;

/// The window's parity step: `(sampled, sampled_ok)` for the composed BAL —
/// zero when the profile's schedule is not `LiveParity` (the schedule is the
/// divergence; the contract is shared with the applier's lens).
pub(super) async fn parity_step(
    ctx: &Ctx,
    bal: &mirror_devnet::bal::BlockAccessList,
) -> Result<(usize, usize)> {
    if !matches!(ctx.mirror.policies().probes, ProbeSchedule::LiveParity) {
        return Ok((0, 0));
    }
    // `verify` answers `(ok, failed)`; the fact's contract is `(sample size,
    // passed)` — the applier's naming, so the two mirrors' facts read the
    // same way (found by the venue selftest: the inverted pair reported every
    // clean window as a failure).
    let (ok, failed) = mirror_devnet::bal::verify(ctx.target.as_ref(), bal).await?;
    Ok(sample_pair(ok, failed))
}

/// The fact's pair contract, one expression with a name: `(size, passed)`.
fn sample_pair(ok: usize, failed: usize) -> (usize, usize) {
    (ok + failed, ok)
}

#[cfg(test)]
mod tests {
    use super::sample_pair;

    /// The pair the window's fact carries is `(size, passed)` — never the
    /// inverted `(passed, size)` the venue selftest once caught (every clean
    /// window read as a failure).
    #[test]
    fn the_parity_pair_is_size_then_passed() {
        assert_eq!(sample_pair(3, 1), (4, 3));
        assert_eq!(sample_pair(0, 0), (0, 0));
        assert_eq!(sample_pair(0, 2), (2, 0));
    }
}
