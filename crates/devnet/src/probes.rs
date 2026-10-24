//! `ProbeRunner` — configured state probes, compared source-vs-target.
//!
//! One shape, both mirrors: the devnet probe parity (source = the parent
//! source at the applied block / at the pin, target = anvil T or V). Targets
//! the devnet owns are skipped, counted, never alarmed on — copy-on-write
//! means that state is the devnet's, not the source's.
use crate::error::Result;
use crate::shadow::Shadow;
use crate::shared::Shared;
use alloy_primitives::Address;
use mirror_anvil::Rpc;
use mirror_config::Probe;
use std::collections::HashSet;

/// One probe run's tally. This is also the `/admin/probes` wire shape on both
/// engines (the shared route's response): the handlers serve it directly, so
/// there is no per-crate report struct to drift.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct ProbeOutcome {
    pub ok: usize,
    pub failed: usize,
    pub skipped: usize,
}

/// Report one run: the fact on the bus and the log line, one place for both
/// engines' on-demand paths (`at` names the compared block — the mainnet's
/// cursor, the venue's pin). The applier's per-block probe path counts its
/// own convergence counters and does not report here.
pub fn report(plane: &crate::plane::Plane, chain: &'static str, at: u64, out: &ProbeOutcome) {
    tracing::info!(
        block = at,
        ok = out.ok,
        failed = out.failed,
        skipped = out.skipped,
        "probes complete"
    );
    plane.facts.publish(crate::facts::Fact::ProbesRun {
        chain,
        at,
        ok: out.ok,
        failed: out.failed,
        skipped: out.skipped,
    });
}

pub struct ProbeRunner<'a> {
    probes: &'a [Probe],
    /// Targets the devnet owns (COW): their probes are skipped, never alarmed.
    shadow: &'a Shared<Shadow>,
    /// The devnet clock is offset: every time-dependent view diverges from the
    /// parent by design (that is what the offset is for), so skip instead of
    /// alarming.
    clock_offset: bool,
}

impl<'a> ProbeRunner<'a> {
    /// A devnet chain (T/V): skip devnet-owned targets, and everything when the
    /// clock is offset.
    pub fn devnet(probes: &'a [Probe], shadow: &'a Shared<Shadow>, clock_offset: bool) -> Self {
        Self {
            probes,
            shadow,
            clock_offset,
        }
    }

    /// Run every probe against `source` at `at` and `target` at its latest
    /// state. A probe read that fails counts as failed (the alarm is the
    /// mismatch counter; the fault is logged with its cause).
    pub async fn parity(&self, source: &Rpc, at: u64, target: &Rpc) -> Result<ProbeOutcome> {
        let mut out = ProbeOutcome::default();
        if self.clock_offset {
            // The offset hides every probe; count them so the blind spot is
            // visible instead of reading as "no probes configured".
            out.skipped = self.probes.len();
            tracing::debug!(
                block = at,
                skipped = out.skipped,
                "probes skipped: the devnet clock is offset"
            );
            return Ok(out);
        }
        let shadowed: HashSet<Address> = self
            .shadow
            .lock()?
            .owned_addresses()
            .into_iter()
            .collect();
        for probe in self.probes {
            if shadowed.contains(&probe.to) {
                // A devnet write anywhere in this target hides *all* of its
                // probes (measured: a crashed oracle source stayed marked and
                // the five oracle probes were skipped silently). Count them so
                // the blind spot is visible; hand the key back to check again.
                out.skipped += 1;
                tracing::debug!(block = at, probe = %probe.name, target = ?probe.to,
                    "probe skipped: target is devnet-owned");
                continue;
            }
            if compare_probe(source, target, probe, at).await {
                out.ok += 1;
            } else {
                out.failed += 1;
            }
        }
        Ok(out)
    }
}

/// Compare one probe: the source at `at` vs the target's latest. A read fault
/// counts as a mismatch (the fault is logged here with its cause). One
/// function per probe — the loop above is the tally.
async fn compare_probe(source: &Rpc, target: &Rpc, probe: &Probe, at: u64) -> bool {
    let r = source.call_at(probe.to, &probe.data, at).await;
    let t = target.call_latest(probe.to, &probe.data).await;
    match (r, t) {
        (Ok(a), Ok(b)) if a == b => true,
        (Ok(a), Ok(b)) => {
            tracing::warn!(
                block = at,
                probe = %probe.name,
                source = %mirror_anvil::truncate(&alloy_primitives::hex::encode(&a), 80),
                target = %mirror_anvil::truncate(&alloy_primitives::hex::encode(&b), 80),
                "probe mismatch"
            );
            false
        }
        (Err(e), _) | (_, Err(e)) => {
            tracing::debug!(block = at, probe = %probe.name, error = %e, "probe read failed");
            false
        }
    }
}
