//! `ProbeRunner` — configured state probes, compared source-vs-target.
//!
//! One shape, both mirrors: the devnet probe parity (source = the parent
//! source at the applied block / at the pin, target = anvil T or V). Targets
//! the devnet owns are skipped, counted, never alarmed on — copy-on-write
//! means that state is the devnet's, not the source's.
use crate::error::Result;
use crate::shadow::Shadow;
use crate::shared::Shared;
use alloy_primitives::{Address, Bytes};
use alloy_rpc_types_eth::BlockNumberOrTag;
use diorama_anvil::Rpc;
use diorama_config::Probe;
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

impl ProbeOutcome {
    /// Everything skipped, nothing read — the one spelling for "this run had
    /// nothing to check" (no cursor, no pin, a source that moved off the pin).
    pub fn skipped(n: usize) -> Self {
        Self {
            skipped: n,
            ..Default::default()
        }
    }
}

/// Report one run: the fact on the bus and the log line, one place for both
/// engines' on-demand paths (`at` names the compared block — the mainnet's
/// cursor, the venue's pin). The applier's per-block probe path counts its
/// own convergence counters and does not report here.
/// Run the probe set against `source` at `at`, on the mirror's target: the
/// runner tail both engines share. The *schedule* (per block, at pin) and the
/// skip decisions stay the mirror's; the clock offset is the plane's (the
/// venue never sets it, so the same read is exact there).
pub async fn run_at<E: crate::engine::Engine>(ctx: &E, at: u64) -> crate::Result<ProbeOutcome> {
    ProbeRunner::devnet(
        &ctx.engine().verifier.probes,
        &ctx.plane().shadow,
        match ctx
            .plane()
            .time_offset
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            0 => ClockOffset::Aligned,
            _ => ClockOffset::Offset,
        },
    )
    .parity(ctx.source_rpc(), at, ctx.target())
    .await
}

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
    clock: ClockOffset,
}

/// Whether the devnet's clock is offset from the parent chain's: the probes'
/// skip decision, named (a bare `true` said nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockOffset {
    /// The clocks agree; time-dependent views are comparable.
    Aligned,
    /// The devnet clock is offset; time-dependent views diverge by design.
    Offset,
}

impl<'a> ProbeRunner<'a> {
    /// A devnet chain (T/V): skip devnet-owned targets, and everything when the
    /// clock is offset.
    pub fn devnet(probes: &'a [Probe], shadow: &'a Shared<Shadow>, clock: ClockOffset) -> Self {
        Self {
            probes,
            shadow,
            clock,
        }
    }

    /// Run every probe against `source` at `at` and `target` at its latest
    /// state. A probe read that fails counts as failed (the alarm is the
    /// mismatch counter; the fault is logged with its cause).
    pub async fn parity(&self, source: &Rpc, at: u64, target: &Rpc) -> Result<ProbeOutcome> {
        let mut out = ProbeOutcome::default();
        if matches!(self.clock, ClockOffset::Offset) {
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
        // The reads ride one batch per side — the two sides run concurrently
        // (different endpoints; each keeps the client's one-body-at-a-time
        // discipline), and a batch carries per-item errors, so one probe's
        // read failure still counts only itself. The per-probe form cost two
        // round trips each, serialized.
        let (runnable, skipped) = self.runnable(at)?;
        out.skipped = skipped;
        if runnable.is_empty() {
            return Ok(out);
        }
        let reads: Vec<(Address, Bytes)> = runnable
            .iter()
            .map(|probe| (probe.to, probe.data.clone()))
            .collect();
        let source_calls = Rpc::call_calls(&reads, BlockNumberOrTag::Number(at))?;
        let target_calls = Rpc::call_calls(&reads, BlockNumberOrTag::Latest)?;
        let (source_answers, target_answers) = match tokio::try_join!(
            source.batch(&source_calls),
            target.batch(&target_calls)
        ) {
            Ok(pair) => pair,
            Err(e) => {
                // A transport-level failure (after the client's own retries):
                // every runnable probe counts failed — the per-probe form's
                // own outcome, one line instead of one per probe.
                out.failed += runnable.len();
                tracing::warn!(block = at, probes = runnable.len(), error = %e, "probe batch read failed");
                return Ok(out);
            }
        };
        for ((probe, source_answer), target_answer) in
            runnable.iter().zip(source_answers).zip(target_answers)
        {
            if count_probe(probe, at, source_answer, target_answer) {
                out.ok += 1;
            } else {
                out.failed += 1;
            }
        }
        Ok(out)
    }

    /// The devnet-owned targets, as a set (the probe loop's skip test).
    fn owned_addresses(&self) -> Result<HashSet<Address>> {
        Ok(self.shadow.lock()?.owned_addresses().into_iter().collect())
    }

    /// The runnable probes and the skipped count: a devnet write anywhere in
    /// a target hides *all* of its probes (measured: a crashed oracle source
    /// stayed marked and the five oracle probes were skipped silently) —
    /// counted so the blind spot is visible; hand the key back to check
    /// again.
    fn runnable(&self, at: u64) -> Result<(Vec<&Probe>, usize)> {
        let shadowed = self.owned_addresses()?;
        let mut runnable: Vec<&Probe> = Vec::with_capacity(self.probes.len());
        let mut skipped = 0usize;
        for probe in self.probes {
            if shadowed.contains(&probe.to) {
                skipped += 1;
                tracing::debug!(block = at, probe = %probe.name, target = ?probe.to,
                    "probe skipped: target is devnet-owned");
            } else {
                runnable.push(probe);
            }
        }
        Ok((runnable, skipped))
    }
}

/// Compare one probe: the source at `at` vs the target's latest. A read fault
/// counts as a mismatch (the fault is logged here with its cause). One
/// function per probe — the loop above is the tally.
/// One probe's pair of batch answers, counted: a decoded match is ok, a
/// decoded mismatch logs the drift, a failed side (or an undecodable answer)
/// counts failed — never a silent skip.
fn count_probe<E: std::fmt::Display>(
    probe: &Probe,
    at: u64,
    source_answer: std::result::Result<Box<serde_json::value::RawValue>, E>,
    target_answer: std::result::Result<Box<serde_json::value::RawValue>, E>,
) -> bool {
    match (source_answer, target_answer) {
        (Ok(a), Ok(b)) => match (decode_call(&a), decode_call(&b)) {
            (Some(a), Some(b)) if a == b => true,
            (Some(a), Some(b)) => {
                log_mismatch(probe, at, &a, &b);
                false
            }
            _ => {
                tracing::debug!(block = at, probe = %probe.name,
                    "probe read failed: an answer did not decode");
                false
            }
        },
        (Err(e), _) | (_, Err(e)) => {
            tracing::debug!(block = at, probe = %probe.name, error = %e, "probe read failed");
            false
        }
    }
}

/// One `eth_call` answer, decoded: a value that is not a hex byte string (or
/// a null) is a read failure, never a mismatch.
fn decode_call(raw: &serde_json::value::RawValue) -> Option<Bytes> {
    serde_json::from_str(raw.get()).ok()
}

/// A probe mismatch: both sides hex-encoded and truncated (the log's shape).
fn log_mismatch(probe: &Probe, at: u64, source: &[u8], target: &[u8]) {
    tracing::warn!(
        block = at,
        probe = %probe.name,
        source = %diorama_anvil::truncate(&alloy_primitives::hex::encode(source), 80),
        target = %diorama_anvil::truncate(&alloy_primitives::hex::encode(target), 80),
        "probe mismatch"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::Shared;
    use std::sync::Arc;

    /// The offset clock skips every probe, counted, before any read: the
    /// blind spot is visible instead of reading as "no probes configured".
    /// The endpoints are unreachable, so a read would fail loudly.
    #[tokio::test]
    async fn an_offset_clock_skips_every_probe() {
        let rpc = Arc::new(
            diorama_anvil::Rpc::new("http://127.0.0.1:1", std::time::Duration::from_secs(1)).unwrap(),
        );
        let probes = vec![Probe {
            name: "p".into(),
            to: alloy_primitives::Address::ZERO,
            data: alloy_primitives::Bytes::new(),
        }];
        let shadow = Shared::new("shadow", crate::shadow::Shadow::default());
        let out = ProbeRunner::devnet(&probes, &shadow, ClockOffset::Offset)
            .parity(&rpc, 1, &rpc)
            .await
            .unwrap();
        assert_eq!((out.ok, out.failed, out.skipped), (0, 0, 1));
        assert_eq!(ProbeOutcome::skipped(3).skipped, 3);
    }
}
