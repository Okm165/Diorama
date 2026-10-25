//! Prometheus-text metrics (hand-rolled: one less dependency for a handful of
//! counters, and the engine's metric surface is deliberately small).
use std::sync::atomic::{AtomicU64, Ordering};

/// One Prometheus text block (HELP/TYPE/value). The shared shape for every
/// registry's render (the engine's `Metrics`, the venue's `VenueMetrics`).
/// The Prometheus kind vocabulary, named once: a typo'd kind would render a
/// wrong `# TYPE` line silently (the repeated-literal reading's finding).
pub const COUNTER: &str = "counter";
pub const GAUGE: &str = "gauge";

pub fn push_metric(out: &mut String, kind: &str, name: &str, help: &str, v: u64) {
    push_metric_line(
        out,
        &MetricLine {
            kind,
            name,
            help,
            labels: &[],
            value: v,
        },
    );
}

/// One exposition line's fields: the render's one shape, labeled series
/// included. The venue's feed used to carry a second implementation of this
/// exact format; one home now (`push_metric` is the unlabeled shorthand).
pub struct MetricLine<'a> {
    pub kind: &'a str,
    pub name: &'a str,
    pub help: &'a str,
    pub labels: &'a [(&'a str, &'a str)],
    pub value: u64,
}

/// The labeled form: `name{k="v",…} value`, with its `# HELP`/`# TYPE`
/// header (the same bytes the unlabeled shorthand renders).
pub fn push_metric_line(out: &mut String, m: &MetricLine<'_>) {
    let MetricLine {
        kind,
        name,
        help,
        labels,
        value,
    } = m;
    let series = if labels.is_empty() {
        name.to_string()
    } else {
        let l = labels
            .iter()
            .map(|(k, v)| format!("{k}=\"{v}\""))
            .collect::<Vec<_>>()
            .join(",");
        format!("{name}{{{l}}}")
    };
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n{series} {value}\n"));
}

/// Declare a registry: one line per series, expanding to the atomic fields and
/// the declared exposition. The field, the series name and the render line can
/// never drift apart — the pattern used to be written twice per metric (a
/// field and a render line), and a missed pair was a silently unreported
/// series.
///
/// A pair may carry a unit divisor (`field = "series", "help" / 1000`), for a
/// series the field stores in a finer unit. The generated `render_declared`
/// writes the declared lines in declaration order; a registry with a computed
/// series (the mainnet engine's `mirror_lag_blocks`, a difference of two
/// fields) writes its own `render` beside it.
#[macro_export]
macro_rules! registry {
    (
        $(#[$sattr:meta])*
        pub struct $name:ident {
            $(
                $(#[$fattr:meta])*
                $kind:ident $field:ident = $series:literal, $help:literal $(/ $div:literal)?;
            )*
        }
    ) => {
        $(#[$sattr])*
        #[derive(Default)]
        pub struct $name {
            $(
                $(#[$fattr])*
                pub $field: std::sync::atomic::AtomicU64,
            )*
        }

        impl $name {
            /// The declared series, in declaration order.
            fn render_declared(&self, out: &mut String) {
                $(
                    $crate::metrics::push_metric(
                        out,
                        stringify!($kind),
                        $series,
                        $help,
                        self.$field.load(std::sync::atomic::Ordering::Relaxed) $(/ $div)?,
                    );
                )*
            }
        }
    };
}

registry! {
    /// The shared admin surface's gauges (plan §11.3): what the devnet plane
    /// itself owns. Each profile renders its own set beside these (the mainnet
    /// engine's `MainnetMetrics`, the venue's `VenueMetrics`), so this crate
    /// knows nothing about reth, T or the venue's chain.
    pub struct Metrics {
        gauge shadowed_slots = "mirror_shadowed_slots", "storage slots shadowed by the devnet";
        gauge shadowed_accounts = "mirror_shadowed_accounts", "accounts with devnet-owned keys (whole-account marks or per-slot marks)";
        gauge halted = "mirror_halted", "1 while the driver is halted";
    }
}

/// The registry convention, one implementation for every registry: the shared
/// admin gauges, the mainnet profile's `MainnetMetrics`, the venue's
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
    /// Record one shadow scan's work: the walked/empty pair every profile
    /// reports (the applier's inline scan, both drivers' loops, the
    /// hand-backs' pre-scans — five call sites across the two projects, one
    /// definition of what the pair means).
    fn record_scan(&self, walk: &crate::scan::ScanWalk, scanned: &AtomicU64, skipped_empty: &AtomicU64) {
        self.add(scanned, walk.walked);
        self.add(skipped_empty, walk.empty);
    }
}

impl Registry for Metrics {}

impl Metrics {
    /// The shared admin gauges: the whole of this crate's exposition. Each
    /// profile renders these beside its own set (the mainnet engine's
    /// `MainnetMetrics`, the venue's `VenueMetrics`).
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(256);
        self.render_declared(&mut out);
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
    /// profile sets live with their profile (the mainnet engine's
    /// `MainnetMetrics`, the venue's `VenueMetrics`), which also keeps the
    /// compositions duplicate-free (measured 2026-10-23: the shared set once
    /// carried profile gauges beside its admin gauges, and the two copies'
    /// HELP text had drifted).
    #[test]
    fn the_shared_set_is_exactly_the_admin_gauges() {
        let m = Metrics::default();
        let names = series(&m.render());
        let want: BTreeSet<String> =
            ["mirror_halted", "mirror_shadowed_slots", "mirror_shadowed_accounts"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(names, want);
    }

    /// The labeled form renders the same bytes as the unlabeled shorthand,
    /// with the series carrying its labels — one formatter for both (the
    /// venue's feed used to carry a second copy of this format).
    #[test]
    fn labeled_metric_lines_render() {
        let mut out = String::new();
        push_metric(&mut out, "gauge", "x_total", "help", 7);
        assert_eq!(out, "# HELP x_total help\n# TYPE x_total gauge\nx_total 7\n");
        out.clear();
        push_metric_line(
            &mut out,
            &MetricLine {
                kind: "counter",
                name: "x_total",
                help: "help",
                labels: &[("coin", "ETH")],
                value: 9,
            },
        );
        assert_eq!(
            out,
            "# HELP x_total help\n# TYPE x_total counter\nx_total{coin=\"ETH\"} 9\n"
        );
    }
}
