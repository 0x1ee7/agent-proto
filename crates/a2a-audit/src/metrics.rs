//! Metric registry with bounded label sets and Prometheus text rendering (RFC-0013 0013-033,
//! 0013-034, 0013-035, table 4.8).
//!
//! Every metric is declared in [`SPECS`]. Labels are fixed per metric, label values are short
//! tokens, and the total number of series is capped, so no identifier that varies per task,
//! event or request can grow the label space.

use std::collections::BTreeMap;

/// Metric kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Monotonic counter.
    Counter,
    /// Settable gauge.
    Gauge,
    /// Bucketed histogram.
    Histogram,
}

impl Kind {
    fn type_name(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

/// Declaration of one metric family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricSpec {
    /// Full metric name, `a2abus_` prefixed.
    pub name: &'static str,
    /// Metric kind.
    pub kind: Kind,
    /// Label names, in the order values are supplied.
    pub labels: &'static [&'static str],
    /// HELP text.
    pub help: &'static str,
}

/// Every metric the crate may record (table 4.8).
pub static SPECS: &[MetricSpec] = &[
    MetricSpec {
        name: "a2abus_task_transitions_total",
        kind: Kind::Counter,
        labels: &["agent", "state"],
        help: "Task state transitions.",
    },
    MetricSpec {
        name: "a2abus_task_duration_seconds",
        kind: Kind::Histogram,
        labels: &["agent", "outcome"],
        help: "Task duration from submission to terminal state.",
    },
    MetricSpec {
        name: "a2abus_tasks_inflight",
        kind: Kind::Gauge,
        labels: &["agent"],
        help: "Tasks currently in progress.",
    },
    MetricSpec {
        name: "a2abus_approvals_total",
        kind: Kind::Counter,
        labels: &["agent", "decision"],
        help: "Approval decisions.",
    },
    MetricSpec {
        name: "a2abus_approval_latency_seconds",
        kind: Kind::Histogram,
        labels: &["agent"],
        help: "Time from approval request to decision.",
    },
    MetricSpec {
        name: "a2abus_budget_debited_total",
        kind: Kind::Counter,
        labels: &["agent", "budget_kind"],
        help: "Budget units debited.",
    },
    MetricSpec {
        name: "a2abus_budget_trips_total",
        kind: Kind::Counter,
        labels: &["agent", "budget_kind"],
        help: "Budget limit trips.",
    },
    MetricSpec {
        name: "a2abus_loop_trips_total",
        kind: Kind::Counter,
        labels: &["agent"],
        help: "Loop detector trips.",
    },
    MetricSpec {
        name: "a2abus_signature_failures_total",
        kind: Kind::Counter,
        labels: &["component", "reason"],
        help: "Signature verification failures.",
    },
    MetricSpec {
        name: "a2abus_peer_rejections_total",
        kind: Kind::Counter,
        labels: &["reason"],
        help: "Peer messages rejected.",
    },
    MetricSpec {
        name: "a2abus_process_state",
        kind: Kind::Gauge,
        labels: &["agent", "state"],
        help: "Process state, one series per agent set to 1.",
    },
    MetricSpec {
        name: "a2abus_restarts_total",
        kind: Kind::Counter,
        labels: &["agent"],
        help: "Agent process restarts.",
    },
    MetricSpec {
        name: "a2abus_workspaces",
        kind: Kind::Gauge,
        labels: &["agent", "state"],
        help: "Workspaces by state.",
    },
    MetricSpec {
        name: "a2abus_workspace_refusals_total",
        kind: Kind::Counter,
        labels: &["reason"],
        help: "Workspace requests refused.",
    },
    MetricSpec {
        name: "a2abus_presence_agents",
        kind: Kind::Gauge,
        labels: &["state"],
        help: "Agents by presence state.",
    },
    MetricSpec {
        name: "a2abus_audit_events_total",
        kind: Kind::Counter,
        labels: &["type"],
        help: "Audit events emitted.",
    },
    MetricSpec {
        name: "a2abus_audit_dropped_total",
        kind: Kind::Counter,
        labels: &["emitter"],
        help: "Audit events discarded from a full spool.",
    },
    MetricSpec {
        name: "a2abus_audit_sink_lag_events",
        kind: Kind::Gauge,
        labels: &[],
        help: "Audit events spooled but not yet written to the sink.",
    },
    MetricSpec {
        name: "a2abus_bus_errors_total",
        kind: Kind::Counter,
        labels: &["component", "kind"],
        help: "Bus errors.",
    },
];

/// Histogram upper bounds, in seconds.
pub const HISTOGRAM_BUCKETS: &[f64] = &[0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0, 900.0, 3600.0];

/// Maximum number of series across all metrics.
pub const SERIES_LIMIT: usize = 512;

/// Maximum length of one label value.
const LABEL_VALUE_MAX: usize = 64;

/// Errors from recording a metric.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MetricsError {
    /// No metric with this name is declared in [`SPECS`].
    #[error("unknown metric {0}")]
    UnknownMetric(String),
    /// The recording call does not match the metric's kind.
    #[error("metric {0} does not accept this operation")]
    WrongKind(&'static str),
    /// The number of label values differs from the declared label names.
    #[error("metric {0} takes {1} label value(s)")]
    LabelCount(&'static str, usize),
    /// A label value is outside the bounded token form.
    #[error("label {label} value {value:?} is not a bounded token")]
    LabelValue {
        /// Label name.
        label: &'static str,
        /// Rejected value.
        value: String,
    },
    /// Adding this series would exceed [`SERIES_LIMIT`].
    #[error("series limit of {SERIES_LIMIT} reached")]
    SeriesLimit,
    /// Observed value is NaN or infinite.
    #[error("metric {0} observed a non-finite value")]
    NonFinite(&'static str),
}

#[derive(Debug, Clone, PartialEq)]
enum Series {
    Counter(u64),
    Gauge(f64),
    Histogram {
        /// Non-cumulative counts per bucket; observations above the last bound only add to `count`.
        buckets: Vec<u64>,
        sum: f64,
        count: u64,
    },
}

impl Series {
    fn new(kind: Kind) -> Self {
        match kind {
            Kind::Counter => Self::Counter(0),
            Kind::Gauge => Self::Gauge(0.0),
            Kind::Histogram => Self::Histogram {
                buckets: vec![0; HISTOGRAM_BUCKETS.len()],
                sum: 0.0,
                count: 0,
            },
        }
    }
}

/// Registry of series for every declared metric.
#[derive(Debug, Default)]
pub struct Metrics {
    series: BTreeMap<&'static str, BTreeMap<Vec<String>, Series>>,
    total: usize,
}

impl Metrics {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `by` to a counter series.
    pub fn inc(&mut self, name: &str, labels: &[&str], by: u64) -> Result<(), MetricsError> {
        let (_, series) = self.locate(name, labels, Kind::Counter)?;
        if let Series::Counter(value) = series {
            *value = value.saturating_add(by);
        }
        Ok(())
    }

    /// Sets a gauge series.
    pub fn set(&mut self, name: &str, labels: &[&str], value: f64) -> Result<(), MetricsError> {
        let (spec, series) = self.locate(name, labels, Kind::Gauge)?;
        if !value.is_finite() {
            return Err(MetricsError::NonFinite(spec.name));
        }
        if let Series::Gauge(current) = series {
            *current = value;
        }
        Ok(())
    }

    /// Records one observation in a histogram series.
    pub fn observe(&mut self, name: &str, labels: &[&str], value: f64) -> Result<(), MetricsError> {
        let (spec, series) = self.locate(name, labels, Kind::Histogram)?;
        if !value.is_finite() {
            return Err(MetricsError::NonFinite(spec.name));
        }
        if let Series::Histogram {
            buckets,
            sum,
            count,
        } = series
        {
            if let Some(slot) = HISTOGRAM_BUCKETS.iter().position(|bound| value <= *bound) {
                buckets[slot] += 1;
            }
            *sum += value;
            *count += 1;
        }
        Ok(())
    }

    /// Current value of a counter series, or `None` if the series does not exist.
    pub fn counter(&self, name: &str, labels: &[&str]) -> Option<u64> {
        match self.get(name, labels)? {
            Series::Counter(value) => Some(*value),
            _ => None,
        }
    }

    /// Number of series currently held.
    pub fn series_count(&self) -> usize {
        self.total
    }

    /// Renders every declared metric in the Prometheus text exposition format.
    pub fn render(&self) -> String {
        let mut lines = Vec::new();
        for spec in SPECS {
            lines.push(format!("# HELP {} {}", spec.name, spec.help));
            lines.push(format!("# TYPE {} {}", spec.name, spec.kind.type_name()));
            let Some(family) = self.series.get(spec.name) else {
                continue;
            };
            for (values, series) in family {
                let labels = label_text(spec.labels, values);
                match series {
                    Series::Counter(value) => {
                        lines.push(format!("{}{} {}", spec.name, braces(&labels), value));
                    }
                    Series::Gauge(value) => {
                        lines.push(format!("{}{} {}", spec.name, braces(&labels), value));
                    }
                    Series::Histogram {
                        buckets,
                        sum,
                        count,
                    } => {
                        let mut cumulative = 0;
                        for (bound, in_bucket) in HISTOGRAM_BUCKETS.iter().zip(buckets) {
                            cumulative += in_bucket;
                            let with_le = join_labels(&labels, &format!("le=\"{bound}\""));
                            lines.push(format!("{}_bucket{{{with_le}}} {cumulative}", spec.name));
                        }
                        let inf = join_labels(&labels, "le=\"+Inf\"");
                        lines.push(format!("{}_bucket{{{inf}}} {count}", spec.name));
                        lines.push(format!("{}_sum{} {sum}", spec.name, braces(&labels)));
                        lines.push(format!("{}_count{} {count}", spec.name, braces(&labels)));
                    }
                }
            }
        }
        lines.push(String::new());
        lines.join("\n")
    }

    fn get(&self, name: &str, labels: &[&str]) -> Option<&Series> {
        let spec = spec_named(name)?;
        let key: Vec<String> = labels.iter().map(|value| (*value).to_owned()).collect();
        self.series.get(spec.name)?.get(&key)
    }

    fn locate(
        &mut self,
        name: &str,
        labels: &[&str],
        kind: Kind,
    ) -> Result<(&'static MetricSpec, &mut Series), MetricsError> {
        let spec = spec_named(name).ok_or_else(|| MetricsError::UnknownMetric(name.to_owned()))?;
        if spec.kind != kind {
            return Err(MetricsError::WrongKind(spec.name));
        }
        if labels.len() != spec.labels.len() {
            return Err(MetricsError::LabelCount(spec.name, spec.labels.len()));
        }
        for (label, value) in spec.labels.iter().zip(labels) {
            if !is_bounded_token(value) {
                return Err(MetricsError::LabelValue {
                    label,
                    value: (*value).to_owned(),
                });
            }
        }
        let key: Vec<String> = labels.iter().map(|value| (*value).to_owned()).collect();
        let at_limit = self.total >= SERIES_LIMIT;
        let family = self.series.entry(spec.name).or_default();
        if !family.contains_key(&key) {
            if at_limit {
                return Err(MetricsError::SeriesLimit);
            }
            self.total += 1;
        }
        let series = family.entry(key).or_insert_with(|| Series::new(kind));
        Ok((spec, series))
    }
}

fn spec_named(name: &str) -> Option<&'static MetricSpec> {
    SPECS.iter().find(|spec| spec.name == name)
}

/// A label value is 1 to 64 characters from `[A-Za-z0-9_.-]`, and is not UUID-shaped or a
/// 32-character hex string, which would indicate a per-task or per-event identifier.
fn is_bounded_token(value: &str) -> bool {
    let shape_ok = !value.is_empty()
        && value.len() <= LABEL_VALUE_MAX
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-');
    shape_ok && !looks_like_identifier(value)
}

fn looks_like_identifier(value: &str) -> bool {
    let hex32 = value.len() == 32 && value.bytes().all(|b| b.is_ascii_hexdigit());
    let uuid = value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        });
    hex32 || uuid
}

fn label_text(names: &[&str], values: &[String]) -> String {
    names
        .iter()
        .zip(values)
        .map(|(name, value)| format!("{name}=\"{value}\""))
        .collect::<Vec<_>>()
        .join(",")
}

fn braces(labels: &str) -> String {
    if labels.is_empty() {
        String::new()
    } else {
        format!("{{{labels}}}")
    }
}

fn join_labels(labels: &str, extra: &str) -> String {
    if labels.is_empty() {
        extra.to_owned()
    } else {
        format!("{labels},{extra}")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn req_0013_034_label_values_must_be_bounded_tokens() {
        let long = "x".repeat(65);
        let mut metrics = Metrics::new();
        assert_eq!(
            metrics.inc("a2abus_restarts_total", &[], 1),
            Err(MetricsError::LabelCount("a2abus_restarts_total", 1))
        );
        assert!(
            metrics
                .inc("a2abus_restarts_total", &["agent-a"], 1)
                .is_ok()
        );
        for bad in [
            "",
            "has space",
            "slash/path",
            "7f9c2ba4-e88f-4b9e-9c3f-2d1e0a9b8c7d",
            "7f9c2ba4e88f4b9e9c3f2d1e0a9b8c7d",
            long.as_str(),
        ] {
            assert!(
                matches!(
                    metrics.inc("a2abus_restarts_total", &[bad], 1),
                    Err(MetricsError::LabelValue { label: "agent", .. })
                ),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn req_0013_034_series_are_capped() {
        let mut metrics = Metrics::new();
        // 19 metrics exist; use one label-bearing metric for the cap.
        for i in 0..SERIES_LIMIT {
            let label = format!("r{i}");
            metrics
                .inc("a2abus_peer_rejections_total", &[label.as_str()], 1)
                .unwrap();
        }
        assert_eq!(metrics.series_count(), SERIES_LIMIT);
        assert_eq!(
            metrics.inc("a2abus_peer_rejections_total", &["overflow"], 1),
            Err(MetricsError::SeriesLimit)
        );
        metrics
            .inc("a2abus_peer_rejections_total", &["r0"], 1)
            .unwrap();
        assert_eq!(
            metrics.counter("a2abus_peer_rejections_total", &["r0"]),
            Some(2)
        );
    }

    #[test]
    fn req_0013_034_kind_and_name_are_checked() {
        let mut metrics = Metrics::new();
        assert_eq!(
            metrics.inc("a2abus_not_declared_total", &[], 1),
            Err(MetricsError::UnknownMetric(
                "a2abus_not_declared_total".to_owned()
            ))
        );
        assert_eq!(
            metrics.inc("a2abus_task_duration_seconds", &["agent-a", "ok"], 1),
            Err(MetricsError::WrongKind("a2abus_task_duration_seconds"))
        );
        assert_eq!(
            metrics.set("a2abus_restarts_total", &["agent-a"], 1.0),
            Err(MetricsError::WrongKind("a2abus_restarts_total"))
        );
        assert_eq!(
            metrics.observe("a2abus_tasks_inflight", &["agent-a"], 1.0),
            Err(MetricsError::WrongKind("a2abus_tasks_inflight"))
        );
    }

    #[test]
    fn req_0013_034_non_finite_values_are_refused() {
        let mut metrics = Metrics::new();
        assert_eq!(
            metrics.set("a2abus_audit_sink_lag_events", &[], f64::NAN),
            Err(MetricsError::NonFinite("a2abus_audit_sink_lag_events"))
        );
        assert_eq!(
            metrics.observe("a2abus_loop_trips_total", &["x"], 1.0),
            Err(MetricsError::WrongKind("a2abus_loop_trips_total"))
        );
    }

    #[test]
    fn req_0013_035_names_follow_convention() {
        let mut seen = BTreeSet::new();
        for spec in SPECS {
            assert!(spec.name.starts_with("a2abus_"), "{}", spec.name);
            assert!(seen.insert(spec.name), "duplicate {}", spec.name);
            assert!(
                spec.name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{}",
                spec.name
            );
            match spec.kind {
                Kind::Counter => assert!(spec.name.ends_with("_total"), "{}", spec.name),
                Kind::Histogram => assert!(spec.name.ends_with("_seconds"), "{}", spec.name),
                Kind::Gauge => assert!(!spec.name.ends_with("_total"), "{}", spec.name),
            }
        }
        assert_eq!(SPECS.len(), 19);
    }

    #[test]
    fn req_0013_035_render_emits_cumulative_histogram_and_counter() {
        let mut metrics = Metrics::new();
        metrics
            .inc("a2abus_restarts_total", &["agent-a"], 2)
            .unwrap();
        metrics
            .observe("a2abus_task_duration_seconds", &["agent-a", "ok"], 0.2)
            .unwrap();
        metrics
            .observe("a2abus_task_duration_seconds", &["agent-a", "ok"], 7.0)
            .unwrap();
        let text = metrics.render();
        assert!(text.contains("# TYPE a2abus_restarts_total counter"));
        assert!(text.contains("a2abus_restarts_total{agent=\"agent-a\"} 2"));
        assert!(text.contains("# TYPE a2abus_task_duration_seconds histogram"));
        assert!(text.contains(
            "a2abus_task_duration_seconds_bucket{agent=\"agent-a\",outcome=\"ok\",le=\"0.5\"} 1"
        ));
        assert!(text.contains(
            "a2abus_task_duration_seconds_bucket{agent=\"agent-a\",outcome=\"ok\",le=\"15\"} 2"
        ));
        assert!(text.contains(
            "a2abus_task_duration_seconds_bucket{agent=\"agent-a\",outcome=\"ok\",le=\"+Inf\"} 2"
        ));
        assert!(
            text.contains("a2abus_task_duration_seconds_count{agent=\"agent-a\",outcome=\"ok\"} 2")
        );
    }

    #[test]
    fn req_0013_034_render_lists_every_declared_metric() {
        let text = Metrics::new().render();
        for spec in SPECS {
            assert!(text.contains(&format!("# TYPE {} {}", spec.name, spec.kind.type_name())));
        }
    }
}
