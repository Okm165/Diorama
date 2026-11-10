//! Prometheus-text metrics (hand-rolled, the workspace convention: a handful
//! of counters needs no dependency). The counters are the bridge's lifecycle
//! (watched → signed → relayed/parked), the per-chain gauges are the relay
//! key's gas (the alerts daloc's relay also carries: 0.1 ETH low / 0.01 ETH
//! critical by default).
use crate::store::Counts;
use alloy_primitives::U256;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Default)]
pub struct CctpMetrics {
    /// `MessageSent` logs absorbed (new records).
    pub messages_seen: AtomicU64,
    /// Records transitioned PENDING → SIGNED.
    pub messages_signed: AtomicU64,
    /// `receiveMessage` landed.
    pub relays_confirmed: AtomicU64,
    /// Definitive failures parked for an operator.
    pub relays_parked: AtomicU64,
    /// Signed records re-checked by the relayer (one per tick per record).
    pub relay_attempts: AtomicU64,
    /// Scan passes that failed (logged with the chain).
    pub scan_errors: AtomicU64,
    /// Gas-alert crossings (each observation below the threshold counts).
    pub gas_low: AtomicU64,
    pub gas_critical: AtomicU64,
    /// The last head the watcher scanned (a coarse liveness gauge).
    pub last_scan_block: AtomicU64,
    /// Parked records read from the durable archives (outside the live store).
    pub archived_parked: AtomicU64,
    /// Archive files that exist but could not be parsed (reported, not
    /// silently dropped).
    pub archives_corrupt: AtomicU64,
    /// The relay key's native balance per chain (wei, full-width: a devnet
    /// balance can exceed `u64`, so the gauge carries the decimal string).
    balances: Mutex<Vec<(String, String)>>,
}

impl CctpMetrics {
    pub fn set_balance(&self, chain: &str, wei: U256) {
        let rendered = wei.to_string();
        let mut balances = self
            .balances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match balances.iter_mut().find(|(name, _)| name == chain) {
            Some((_, value)) => *value = rendered,
            None => balances.push((chain.to_string(), rendered)),
        }
    }

    #[allow(clippy::too_many_lines)] // one rendering/transition table: splitting it would hide the field-to-line mapping
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(1024);
        let counter = |out: &mut String, name: &str, help: &str, value: u64| {
            out.push_str(&format!(
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}\n"
            ));
        };
        counter(
            &mut out,
            "cctp_messages_seen_total",
            "MessageSent logs absorbed",
            self.messages_seen.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "cctp_messages_signed_total",
            "messages transitioned PENDING -> SIGNED",
            self.messages_signed.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "cctp_relays_confirmed_total",
            "messages confirmed relayed (a consumed nonce reconciles as confirmed)",
            self.relays_confirmed.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "cctp_relays_parked_total",
            "messages parked on a definitive failure",
            self.relays_parked.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "cctp_relay_attempts_total",
            "signed messages re-checked by the relayer",
            self.relay_attempts.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "cctp_scan_errors_total",
            "watcher scan passes that failed",
            self.scan_errors.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "cctp_gas_low_total",
            "gas observations below the low threshold",
            self.gas_low.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "cctp_gas_critical_total",
            "gas observations below the critical threshold",
            self.gas_critical.load(Ordering::Relaxed),
        );
        out.push_str(&format!(
            "# HELP cctp_last_scan_block last chain head scanned by the watcher\n# TYPE cctp_last_scan_block gauge\ncctp_last_scan_block {}\n",
            self.last_scan_block.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "# HELP cctp_messages_archived_parked parked records in the durable archives (outside the live store)\n# TYPE cctp_messages_archived_parked gauge\ncctp_messages_archived_parked {}\n",
            self.archived_parked.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "# HELP cctp_archives_corrupt archive files that could not be parsed\n# TYPE cctp_archives_corrupt gauge\ncctp_archives_corrupt {}\n",
            self.archives_corrupt.load(Ordering::Relaxed)
        ));
        let balances = self
            .balances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !balances.is_empty() {
            out.push_str(
                "# HELP cctp_relay_balance_wei the relay key's native balance per chain\n# TYPE cctp_relay_balance_wei gauge\n",
            );
            for (chain, wei) in balances.iter() {
                out.push_str(&format!(
                    "cctp_relay_balance_wei{{chain=\"{chain}\"}} {wei}\n"
                ));
            }
        }
        out
    }
}

/// The store-derived gauges (what the records say right now), rendered beside
/// the counters.
pub(crate) fn render_counts(out: &mut String, counts: &Counts) {
    out.push_str(&format!(
        "# HELP cctp_messages_pending the emulated PENDING window has not elapsed\n# TYPE cctp_messages_pending gauge\ncctp_messages_pending {}\n",
        counts.pending
    ));
    out.push_str(&format!(
        "# HELP cctp_messages_signed signed and waiting for the relayer\n# TYPE cctp_messages_signed gauge\ncctp_messages_signed {}\n",
        counts.signed
    ));
    out.push_str(&format!(
        "# HELP cctp_messages_parked parked failures (never dropped)\n# TYPE cctp_messages_parked gauge\ncctp_messages_parked {}\n",
        counts.parked
    ));
}
