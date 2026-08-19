//! CloudWatch Embedded Metric Format recorder.
//!
//! Lambda has no metrics agent and no sidecar: the only zero-infrastructure
//! path from an in-process `metrics::` counter to a CloudWatch metric is an
//! EMF JSON line on stdout, which the log subscription turns into metrics
//! without any API call. This installs a global `metrics` recorder that
//! accumulates per invocation and flushes one line per label set.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use metrics::{
    Counter, CounterFn, Gauge, GaugeFn, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder,
    SharedString, Unit,
};

/// EMF caps a metric value array at 100 entries.
const MAX_VALUES: usize = 100;
/// EMF caps one metric directive at 100 metric definitions.
const MAX_METRICS_PER_LINE: usize = 100;
/// Retained samples per histogram; count and sum stay exact past this.
const MAX_SAMPLES: usize = 4096;

static ACTIVE: OnceLock<Active> = OnceLock::new();

struct Active {
    registry: Arc<Registry>,
    namespace: String,
    dimensions: Vec<(String, String)>,
}

/// Installs the recorder when `ABGEN_EMF_NAMESPACE` is set; returns whether it
/// is active. Off by default so local runs stay quiet.
pub fn init() -> bool {
    let Some(namespace) = std::env::var("ABGEN_EMF_NAMESPACE")
        .ok()
        .filter(|v| !v.trim().is_empty())
    else {
        return false;
    };
    if ACTIVE.get().is_some() {
        return true;
    }
    let registry = Arc::new(Registry::default());
    if let Err(e) = metrics::set_global_recorder(EmfRecorder {
        registry: registry.clone(),
    }) {
        eprintln!("emf: failed to install recorder: {e}");
        return false;
    }
    let mut dimensions = Vec::new();
    if let Ok(name) = std::env::var("AWS_LAMBDA_FUNCTION_NAME") {
        if !name.is_empty() {
            dimensions.push(("ServiceName".to_string(), name));
        }
    }
    let _ = ACTIVE.set(Active {
        registry,
        namespace: namespace.trim().to_string(),
        dimensions,
    });
    true
}

/// Drains everything recorded since the last flush onto stdout as EMF lines.
pub fn flush() {
    let Some(active) = ACTIVE.get() else {
        return;
    };
    let lines = flush_lines(
        &active.registry,
        &active.namespace,
        &active.dimensions,
        now_ms(),
    );
    if lines.is_empty() {
        return;
    }
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in lines {
        let _ = writeln!(out, "{line}");
    }
    let _ = out.flush();
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct MetricId {
    name: String,
    labels: Vec<(String, String)>,
}

impl MetricId {
    fn from_key(key: &Key) -> Self {
        let mut labels: Vec<(String, String)> = key
            .labels()
            .map(|l| (l.key().to_string(), l.value().to_string()))
            .collect();
        labels.sort();
        MetricId {
            name: key.name().to_string(),
            labels,
        }
    }
}

#[derive(Default)]
struct Registry {
    counters: Mutex<BTreeMap<MetricId, Arc<CounterState>>>,
    gauges: Mutex<BTreeMap<MetricId, Arc<GaugeState>>>,
    histograms: Mutex<BTreeMap<MetricId, Arc<HistogramState>>>,
}

#[derive(Default)]
struct CounterState(AtomicU64);

impl CounterFn for CounterState {
    fn increment(&self, value: u64) {
        self.0.fetch_add(value, Ordering::Relaxed);
    }
    fn absolute(&self, value: u64) {
        self.0.store(value, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct GaugeState(AtomicU64);

impl GaugeState {
    fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }
    fn take(&self) -> f64 {
        f64::from_bits(self.0.swap(0.0f64.to_bits(), Ordering::Relaxed))
    }
    fn update(&self, f: impl Fn(f64) -> f64) {
        let mut current = self.0.load(Ordering::Relaxed);
        loop {
            let next = f(f64::from_bits(current)).to_bits();
            match self
                .0
                .compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }
}

impl GaugeFn for GaugeState {
    fn increment(&self, value: f64) {
        self.update(|v| v + value);
    }
    fn decrement(&self, value: f64) {
        self.update(|v| v - value);
    }
    fn set(&self, value: f64) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }
}

#[derive(Default)]
struct HistogramState {
    samples: Mutex<Vec<f64>>,
    count: AtomicU64,
    sum: GaugeState,
}

impl HistogramFn for HistogramState {
    fn record(&self, value: f64) {
        if !value.is_finite() {
            return;
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum.update(|v| v + value);
        let mut samples = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        if samples.len() < MAX_SAMPLES {
            samples.push(value);
        }
    }
}

struct EmfRecorder {
    registry: Arc<Registry>,
}

fn handle<T: Default>(map: &Mutex<BTreeMap<MetricId, Arc<T>>>, key: &Key) -> Arc<T> {
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(MetricId::from_key(key)).or_default().clone()
}

impl Recorder for EmfRecorder {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        Counter::from_arc(handle(&self.registry.counters, key))
    }
    fn register_gauge(&self, key: &Key, _: &Metadata<'_>) -> Gauge {
        Gauge::from_arc(handle(&self.registry.gauges, key))
    }
    fn register_histogram(&self, key: &Key, _: &Metadata<'_>) -> Histogram {
        Histogram::from_arc(handle(&self.registry.histograms, key))
    }
}

fn unit_for(name: &str) -> &'static str {
    let base = name.strip_suffix("_total").unwrap_or(name);
    if base.ends_with("_seconds") {
        "Seconds"
    } else if base.ends_with("_bytes") {
        "Bytes"
    } else {
        "Count"
    }
}

/// Sorted, evenly spaced picks keep the min, the max, and the shape of the
/// distribution CloudWatch computes percentiles from, within EMF's 100-value
/// array limit.
fn downsample(mut values: Vec<f64>) -> Vec<f64> {
    if values.len() <= MAX_VALUES {
        return values;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let last = values.len() - 1;
    (0..MAX_VALUES)
        .map(|i| values[i * last / (MAX_VALUES - 1)])
        .collect()
}

struct Emitted {
    labels: Vec<(String, String)>,
    name: String,
    unit: &'static str,
    value: serde_json::Value,
}

fn flush_lines(
    registry: &Registry,
    namespace: &str,
    static_dimensions: &[(String, String)],
    timestamp_ms: u64,
) -> Vec<String> {
    let mut emitted: Vec<Emitted> = Vec::new();

    for (id, state) in registry
        .counters
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
    {
        let value = state.0.swap(0, Ordering::Relaxed);
        if value == 0 {
            continue;
        }
        emitted.push(Emitted {
            labels: id.labels.clone(),
            name: id.name.clone(),
            unit: unit_for(&id.name),
            value: value.into(),
        });
    }

    for (id, state) in registry
        .gauges
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
    {
        let value = state.get();
        if !value.is_finite() {
            continue;
        }
        emitted.push(Emitted {
            labels: id.labels.clone(),
            name: id.name.clone(),
            unit: unit_for(&id.name),
            value: value.into(),
        });
    }

    for (id, state) in registry
        .histograms
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
    {
        let count = state.count.swap(0, Ordering::Relaxed);
        if count == 0 {
            continue;
        }
        let sum = state.sum.take();
        let samples = std::mem::take(&mut *state.samples.lock().unwrap_or_else(|e| e.into_inner()));
        let unit = unit_for(&id.name);
        emitted.push(Emitted {
            labels: id.labels.clone(),
            name: id.name.clone(),
            unit,
            value: downsample(samples).into(),
        });
        emitted.push(Emitted {
            labels: id.labels.clone(),
            name: format!("{}_sum", id.name),
            unit,
            value: sum.into(),
        });
        emitted.push(Emitted {
            labels: id.labels.clone(),
            name: format!("{}_count", id.name),
            unit: "Count",
            value: count.into(),
        });
    }

    let mut by_labels: BTreeMap<Vec<(String, String)>, Vec<Emitted>> = BTreeMap::new();
    for e in emitted {
        by_labels.entry(e.labels.clone()).or_default().push(e);
    }

    let mut lines = Vec::new();
    for (labels, mut group) in by_labels {
        group.sort_by(|a, b| a.name.cmp(&b.name));
        let dimension_names: Vec<String> = static_dimensions
            .iter()
            .map(|(k, _)| k.clone())
            .chain(labels.iter().map(|(k, _)| k.clone()))
            .collect();
        for chunk in group.chunks(MAX_METRICS_PER_LINE) {
            let mut line = serde_json::Map::new();
            line.insert(
                "_aws".to_string(),
                serde_json::json!({
                    "Timestamp": timestamp_ms,
                    "CloudWatchMetrics": [{
                        "Namespace": namespace,
                        "Dimensions": [dimension_names],
                        "Metrics": chunk.iter().map(|e| serde_json::json!({
                            "Name": e.name, "Unit": e.unit
                        })).collect::<Vec<_>>(),
                    }],
                }),
            );
            for (k, v) in static_dimensions.iter().chain(labels.iter()) {
                line.insert(k.clone(), v.as_str().into());
            }
            for e in chunk {
                line.insert(e.name.clone(), e.value.clone());
            }
            lines.push(serde_json::Value::Object(line).to_string());
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics::{Key, Label};

    fn line_of(lines: &[String], contains: &str) -> serde_json::Value {
        let raw = lines
            .iter()
            .find(|l| l.contains(contains))
            .unwrap_or_else(|| panic!("no line containing {contains:?} in {lines:#?}"));
        serde_json::from_str(raw).expect("EMF line is JSON")
    }

    fn counter(registry: &Registry, key: &Key) -> Arc<CounterState> {
        handle(&registry.counters, key)
    }

    fn histogram(registry: &Registry, key: &Key) -> Arc<HistogramState> {
        handle(&registry.histograms, key)
    }

    #[test]
    fn empty_registry_emits_nothing() {
        let registry = Registry::default();
        assert!(flush_lines(&registry, "abgen", &[], 1).is_empty());
    }

    #[test]
    fn counter_line_shape() {
        let registry = Registry::default();
        let key = Key::from_parts(
            "abgen_space_transfer_bytes_total",
            vec![Label::new("direction", "upload")],
        );
        counter(&registry, &key).increment(4096);

        let lines = flush_lines(
            &registry,
            "abgen/lambda",
            &[("ServiceName".to_string(), "abgen-prod".to_string())],
            1700000000000,
        );
        assert_eq!(lines.len(), 1);
        let v = line_of(&lines, "transfer_bytes");
        assert_eq!(v["abgen_space_transfer_bytes_total"], 4096);
        assert_eq!(v["direction"], "upload");
        assert_eq!(v["ServiceName"], "abgen-prod");
        assert_eq!(v["_aws"]["Timestamp"], 1700000000000u64);
        let directive = &v["_aws"]["CloudWatchMetrics"][0];
        assert_eq!(directive["Namespace"], "abgen/lambda");
        assert_eq!(
            directive["Dimensions"][0],
            serde_json::json!(["ServiceName", "direction"])
        );
        assert_eq!(
            directive["Metrics"][0],
            serde_json::json!({"Name": "abgen_space_transfer_bytes_total", "Unit": "Bytes"})
        );
    }

    #[test]
    fn flush_drains_counters() {
        let registry = Registry::default();
        let key = Key::from_name("abgen_lambda_jobs_total");
        counter(&registry, &key).increment(1);
        assert_eq!(flush_lines(&registry, "abgen", &[], 1).len(), 1);
        assert!(flush_lines(&registry, "abgen", &[], 2).is_empty());
        counter(&registry, &key).increment(2);
        let v = line_of(&flush_lines(&registry, "abgen", &[], 3), "jobs_total");
        assert_eq!(v["abgen_lambda_jobs_total"], 2);
    }

    #[test]
    fn histogram_emits_values_sum_and_count() {
        let registry = Registry::default();
        let key = Key::from_parts(
            "abgen_space_request_duration_seconds",
            vec![Label::new("op", "get")],
        );
        let h = histogram(&registry, &key);
        h.record(0.25);
        h.record(0.75);
        h.record(f64::NAN);

        let lines = flush_lines(&registry, "abgen", &[], 1);
        let v = line_of(&lines, "duration_seconds");
        assert_eq!(
            v["abgen_space_request_duration_seconds"],
            serde_json::json!([0.25, 0.75])
        );
        assert_eq!(v["abgen_space_request_duration_seconds_sum"], 1.0);
        assert_eq!(v["abgen_space_request_duration_seconds_count"], 2);
        assert_eq!(v["op"], "get");
        let units: Vec<&str> = v["_aws"]["CloudWatchMetrics"][0]["Metrics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["Unit"].as_str().unwrap())
            .collect();
        assert_eq!(units, ["Seconds", "Count", "Seconds"]);
        assert!(flush_lines(&registry, "abgen", &[], 2).is_empty());
    }

    #[test]
    fn histogram_arrays_stay_within_emf_limit() {
        let registry = Registry::default();
        let key = Key::from_name("abgen_space_object_bytes");
        let h = histogram(&registry, &key);
        for i in 0..5000 {
            h.record(i as f64);
        }
        let v = line_of(&flush_lines(&registry, "abgen", &[], 1), "object_bytes");
        let values = v["abgen_space_object_bytes"].as_array().unwrap();
        assert_eq!(values.len(), MAX_VALUES);
        assert_eq!(values[0], 0.0);
        assert_eq!(values[MAX_VALUES - 1], (MAX_SAMPLES - 1) as f64);
        assert_eq!(v["abgen_space_object_bytes_count"], 5000);
        assert_eq!(
            v["abgen_space_object_bytes_sum"],
            serde_json::json!(4999.0 * 5000.0 / 2.0)
        );
    }

    #[test]
    fn one_line_per_label_set() {
        let registry = Registry::default();
        for op in ["get", "put"] {
            counter(
                &registry,
                &Key::from_parts("abgen_space_errors_total", vec![Label::new("op", op)]),
            )
            .increment(1);
        }
        counter(&registry, &Key::from_name("abgen_lambda_jobs_total")).increment(1);
        let lines = flush_lines(&registry, "abgen", &[], 1);
        assert_eq!(lines.len(), 3);
        assert_eq!(line_of(&lines, "\"op\":\"put\"")["op"], "put");
        let unlabeled = line_of(&lines, "jobs_total");
        assert_eq!(
            unlabeled["_aws"]["CloudWatchMetrics"][0]["Dimensions"][0],
            serde_json::json!([])
        );
    }

    #[test]
    fn macros_route_through_the_recorder() {
        let registry = Arc::new(Registry::default());
        let recorder = EmfRecorder {
            registry: registry.clone(),
        };
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!("abgen_space_errors_total", "op" => "put").increment(1);
            metrics::histogram!(
                "abgen_space_request_duration_seconds", "result" => "ok", "op" => "put"
            )
            .record(0.5);
        });
        let lines = flush_lines(&registry, "abgen", &[], 1);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            line_of(&lines, "errors_total")["abgen_space_errors_total"],
            1
        );
        let v = line_of(&lines, "duration_seconds");
        assert_eq!(v["abgen_space_request_duration_seconds_count"], 1);
        assert_eq!(
            v["_aws"]["CloudWatchMetrics"][0]["Dimensions"][0],
            serde_json::json!(["op", "result"])
        );
    }

    #[test]
    fn gauges_report_last_value_and_persist() {
        let registry = Registry::default();
        let key = Key::from_name("abgen_bundle_index_entries");
        let g = handle::<GaugeState>(&registry.gauges, &key);
        g.set(3.0);
        g.increment(2.0);
        g.decrement(1.0);
        let v = line_of(&flush_lines(&registry, "abgen", &[], 1), "bundle_index");
        assert_eq!(v["abgen_bundle_index_entries"], 4.0);
        let v = line_of(&flush_lines(&registry, "abgen", &[], 2), "bundle_index");
        assert_eq!(v["abgen_bundle_index_entries"], 4.0);
    }
}
