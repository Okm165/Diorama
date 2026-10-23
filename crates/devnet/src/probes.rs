//! `ProbeRunner` — configured state probes, compared source-vs-target.
//!
//! Two users with the same shape: the M convergence check (source = reth at
//! `n`, target = anvil-m; no devnet writes on M, nothing to skip) and the
//! devnet probe parity (source = reth at the applied parent block, target =
//! anvil-t/v; targets the devnet owns are skipped, counted, never alarmed on -
//! copy-on-write means that state is the devnet's, not the parent's).
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

pub struct ProbeRunner<'a> {
    probes: &'a [Probe],
    /// Targets the devnet owns (COW). `None` on a chain with no devnet writes.
    shadow: Option<&'a Shared<Shadow>>,
    /// The devnet clock is offset: every time-dependent view diverges from the
    /// parent by design (that is what the offset is for), so skip instead of
    /// alarming.
    clock_offset: bool,
}

impl<'a> ProbeRunner<'a> {
    /// A replay chain (M): no devnet writes, no clock offset - nothing to skip.
    pub fn replay(probes: &'a [Probe]) -> Self {
        Self {
            probes,
            shadow: None,
            clock_offset: false,
        }
    }

    /// A devnet chain (T/V): skip devnet-owned targets, and everything when the
    /// clock is offset.
    pub fn devnet(probes: &'a [Probe], shadow: &'a Shared<Shadow>, clock_offset: bool) -> Self {
        Self {
            probes,
            shadow: Some(shadow),
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
        let shadowed: HashSet<Address> = match self.shadow {
            Some(s) => {
                let s = s.lock()?;
                s.owned_addresses().into_iter().collect()
            }
            None => HashSet::new(),
        };
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
            let r = source.call_at(probe.to, &probe.data, at).await;
            let t = target.call_latest(probe.to, &probe.data).await;
            match (r, t) {
                (Ok(a), Ok(b)) if a == b => out.ok += 1,
                (Ok(a), Ok(b)) => {
                    out.failed += 1;
                    tracing::warn!(
                        block = at,
                        probe = %probe.name,
                        source = %mirror_anvil::truncate(&alloy_primitives::hex::encode(&a), 80),
                        target = %mirror_anvil::truncate(&alloy_primitives::hex::encode(&b), 80),
                        "probe mismatch"
                    );
                }
                (Err(e), _) | (_, Err(e)) => {
                    out.failed += 1;
                    tracing::debug!(block = at, probe = %probe.name, error = %e, "probe read failed");
                }
            }
        }
        Ok(out)
    }
}
