//! Prometheus-text metrics (hand-rolled: one less dependency for a handful of
//! counters, and the engine's metric surface is deliberately small).
use std::sync::atomic::{AtomicU64, Ordering};

/// One Prometheus text block (HELP/TYPE/value). The shared shape for every
/// registry's render (the engine's `Metrics`, the venue's `VenueMetrics`).
pub fn push_metric(out: &mut String, kind: &str, name: &str, help: &str, v: u64) {
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {v}\n"));
}

/// A gauge line: the render shorthand every registry uses.
pub fn gauge(out: &mut String, name: &str, help: &str, v: u64) {
    push_metric(out, "gauge", name, help, v);
}

/// A counter line: the render shorthand every registry uses.
pub fn counter(out: &mut String, name: &str, help: &str, v: u64) {
    push_metric(out, "counter", name, help, v);
}

/// The shared admin surface's gauges (plan §11.3): what the devnet plane
/// itself owns. Each profile renders its own set beside these (the ethereum
/// engine's `EthereumMetrics`, the venue's `VenueMetrics`), so this crate
/// knows nothing about reth, M/T or the venue's chain.
#[derive(Default)]
pub struct Metrics {
    pub shadowed_slots: AtomicU64,
    pub halted: AtomicU64,
    pub frozen: AtomicU64,
}

/// The registry convention, one implementation for every registry: the shared
/// admin gauges, the ethereum profile's `EthereumMetrics`, the venue's
/// `VenueMetrics`. `set`/`add`/`inc` take the field as the operand, so the
/// receiver is the namespace, not the state — the call sites read
/// `registry.add(&registry.field, n)` and a profile's registry can never grow
/// its own variant of the convention.
pub trait Registry {
    fn set(&self, field: &AtomicU64, v: u64) {
        field.store(v, Ordering::Relaxed);
    }
    fn add(&self, field: &AtomicU64, v: u64) {
        field.fetch_add(v, Ordering::Relaxed);
    }
    fn inc(&self, field: &AtomicU64) {
        field.fetch_add(1, Ordering::Relaxed);
    }
}

impl Registry for Metrics {}

impl Metrics {
    /// The shared admin gauges: the whole of this crate's exposition. Each
    /// profile renders these beside its own set (the ethereum engine's
    /// `EthereumMetrics`, the venue's `VenueMetrics`).
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(256);
        push_metric(&mut out, "gauge", "mirror_shadowed_slots", "storage slots shadowed by the devnet", self.shadowed_slots.load(Ordering::Relaxed));
        push_metric(&mut out, "gauge", "mirror_halted", "1 while the driver is halted", self.halted.load(Ordering::Relaxed));
        push_metric(&mut out, "gauge", "mirror_frozen", "1 while the driver is frozen", self.frozen.load(Ordering::Relaxed));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The series names a rendered exposition defines (help/type lines and
    /// label sets stripped).
    fn series(rendered: &str) -> BTreeSet<String> {
        rendered
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(|l| l.split([' ', '{']).next().unwrap_or_default().to_string())
            .collect()
    }

    /// The shared crate's whole exposition: the three admin gauges. A fourth
    /// series means a profile field crept back in — the layering rule is that
    /// profile sets live with their profile (the ethereum engine's
    /// `EthereumMetrics`, the venue's `VenueMetrics`), which also keeps the
    /// compositions duplicate-free (measured 2026-10-23: the shared set once
    /// carried profile gauges beside its admin gauges, and the two copies'
    /// HELP text had drifted).
    #[test]
    fn the_shared_set_is_exactly_the_admin_gauges() {
        let m = Metrics::default();
        let names = series(&m.render());
        let want: BTreeSet<String> = ["mirror_frozen", "mirror_halted", "mirror_shadowed_slots"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(names, want);
    }
}
