//! Process-wide counters and histograms, rendered by `dmem serve` at `GET /metrics` in the
//! Prometheus text format. Atomics only, no dependency; the engine records into them in every
//! mode, and only the server exposes them. Aggregate by design: no tenant name, token or record
//! appears in a label.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

/// Upper bounds (seconds) of the latency buckets: from a cached recall to a long embedding.
const BUCKETS: [f64; 15] = [0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0];

/// A cumulative histogram over `BUCKETS`.
pub struct Histogram {
    counts: [AtomicU64; BUCKETS.len()],
    count: AtomicU64,
    sum_ns: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self { counts: std::array::from_fn(|_| AtomicU64::new(0)), count: AtomicU64::new(0), sum_ns: AtomicU64::new(0) }
    }
}

impl Histogram {
    pub fn observe(&self, d: Duration) {
        let secs = d.as_secs_f64();
        for (i, le) in BUCKETS.iter().enumerate() {
            if secs <= *le {
                self.counts[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ns.fetch_add(d.as_nanos().min(u64::MAX as u128) as u64, Ordering::Relaxed);
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// The `_bucket`, `_sum` and `_count` lines for `name` with `labels` (already rendered as
    /// `k="v",...`, or empty).
    fn render(&self, out: &mut String, name: &str, labels: &str) {
        let sep = if labels.is_empty() { "" } else { "," };
        for (i, le) in BUCKETS.iter().enumerate() {
            let _ = writeln!(out, "{name}_bucket{{{labels}{sep}le=\"{le}\"}} {}", self.counts[i].load(Ordering::Relaxed));
        }
        let n = self.count();
        let _ = writeln!(out, "{name}_bucket{{{labels}{sep}le=\"+Inf\"}} {n}");
        let sum = self.sum_ns.load(Ordering::Relaxed) as f64 / 1e9;
        let braces = |l: &str| if l.is_empty() { String::new() } else { format!("{{{l}}}") };
        let _ = writeln!(out, "{name}_sum{} {sum}", braces(labels));
        let _ = writeln!(out, "{name}_count{} {n}", braces(labels));
    }
}

/// Where an embedding ran: `before_lock` (a request's planned texts, embedded before it took the
/// tenant lock) or `under_lock` (a text the engine had to embed itself, holding the lock).
#[derive(Clone, Copy)]
pub enum EmbedPhase {
    BeforeLock,
    UnderLock,
}

/// Which embedding permits a request waited for (`dmem serve` bounds concurrent embedding):
/// `save` for record bodies, `query` for recall queries.
#[derive(Clone, Copy)]
pub enum EmbedPool {
    Save,
    Query,
}

/// The vector-index operation that failed (its discriminant indexes `ZVEC_OPS`).
#[derive(Clone, Copy)]
pub enum ZvecOp {
    Open = 0,
    Migrate = 1,
    Upsert = 2,
    Search = 3,
    Flush = 4,
}

/// The `op` label of each `ZvecOp`, in discriminant order.
const ZVEC_OPS: [&str; 5] = ["open", "migrate", "upsert", "search", "flush"];

/// One route's latency histogram and its responses by status class.
type RouteStats = (Histogram, BTreeMap<&'static str, u64>);

#[derive(Default)]
struct Registry {
    lock_wait: Histogram,
    lock_hold: Histogram,
    embed_before_lock: Histogram,
    embed_under_lock: Histogram,
    permit_wait_save: Histogram,
    permit_wait_query: Histogram,
    zvec_failures: [AtomicU64; 5],
    /// Per route (the matched path, not the raw URI): latency, and responses by status class.
    routes: Mutex<BTreeMap<String, RouteStats>>,
}

static REG: LazyLock<Registry> = LazyLock::new(Registry::default);

/// Time a request waited for its tenant's lock.
pub fn lock_wait(d: Duration) {
    REG.lock_wait.observe(d);
}

/// Time a request held its tenant's lock.
pub fn lock_hold(d: Duration) {
    REG.lock_hold.observe(d);
}

/// One embedding (a record body, all its windows, or a query) and where it ran.
pub fn embed(phase: EmbedPhase, d: Duration) {
    match phase {
        EmbedPhase::BeforeLock => REG.embed_before_lock.observe(d),
        EmbedPhase::UnderLock => REG.embed_under_lock.observe(d),
    }
}

/// Embeddings recorded so far for `phase`.
pub fn embed_count(phase: EmbedPhase) -> u64 {
    match phase {
        EmbedPhase::BeforeLock => REG.embed_before_lock.count(),
        EmbedPhase::UnderLock => REG.embed_under_lock.count(),
    }
}

/// Time a request waited for an embedding permit.
pub fn embed_permit_wait(pool: EmbedPool, d: Duration) {
    match pool {
        EmbedPool::Save => REG.permit_wait_save.observe(d),
        EmbedPool::Query => REG.permit_wait_query.observe(d),
    }
}

/// Permit waits recorded so far for `pool`.
pub fn embed_permit_waits(pool: EmbedPool) -> u64 {
    match pool {
        EmbedPool::Save => REG.permit_wait_save.count(),
        EmbedPool::Query => REG.permit_wait_query.count(),
    }
}

/// A vector-index operation failed (the caller logs it; recall and saves carry on).
pub fn zvec_failure(op: ZvecOp) {
    REG.zvec_failures[op as usize].fetch_add(1, Ordering::Relaxed);
}

/// Failures counted so far for `op`.
pub fn zvec_failures(op: ZvecOp) -> u64 {
    REG.zvec_failures[op as usize].load(Ordering::Relaxed)
}

/// One answered request: its route (matched path) and status code.
pub fn request(route: &str, status: u16, d: Duration) {
    let class = match status {
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
    };
    let mut routes = REG.routes.lock().unwrap_or_else(|p| p.into_inner());
    let entry = routes.entry(route.to_string()).or_default();
    entry.0.observe(d);
    *entry.1.entry(class).or_default() += 1;
}

/// Server-level gauges `render` prints beside the process-wide series.
pub struct Gauges<'a> {
    pub ready: bool,
    pub shutting_down: bool,
    pub in_flight: usize,
    pub tenants_open: usize,
    pub tenants_without_vectors: usize,
    /// The active embedder's name (None = a build without vectors).
    pub embedder: Option<&'a str>,
    /// Embedding permits (saves, queries); None outside `dmem serve`'s vector build.
    pub embed_permits: Option<(usize, usize)>,
}

fn head(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

/// The whole exposition, Prometheus text format 0.0.4.
pub fn render(g: &Gauges) -> String {
    let mut out = String::new();
    head(&mut out, "dmem_build_info", "gauge", "The running dmem version.");
    let _ = writeln!(out, "dmem_build_info{{version=\"{}\"}} 1", env!("CARGO_PKG_VERSION"));
    head(&mut out, "dmem_ready", "gauge", "1 when GET /readyz passes.");
    let _ = writeln!(out, "dmem_ready {}", g.ready as u8);
    head(&mut out, "dmem_shutting_down", "gauge", "1 once a stop signal arrived.");
    let _ = writeln!(out, "dmem_shutting_down {}", g.shutting_down as u8);
    head(&mut out, "dmem_requests_in_flight", "gauge", "Requests being handled.");
    let _ = writeln!(out, "dmem_requests_in_flight {}", g.in_flight);
    head(&mut out, "dmem_tenants_open", "gauge", "Tenants with an open database handle.");
    let _ = writeln!(out, "dmem_tenants_open {}", g.tenants_open);
    head(&mut out, "dmem_tenants_without_vector_index", "gauge", "Opened tenants whose vector index did not open (keyword-only recall).");
    let _ = writeln!(out, "dmem_tenants_without_vector_index {}", g.tenants_without_vectors);
    head(&mut out, "dmem_embedder_info", "gauge", "The active embedder; placeholder=\"true\" is the keyword-equivalent hash fallback.");
    match g.embedder {
        Some(name) => {
            let _ = writeln!(out, "dmem_embedder_info{{name=\"{name}\",placeholder=\"{}\"}} 1", name == "hash");
        }
        None => {
            let _ = writeln!(out, "dmem_embedder_info{{name=\"none\",placeholder=\"false\"}} 1");
        }
    }
    head(&mut out, "dmem_embed_seconds", "histogram", "Embedding time per text; phase=\"under_lock\" held the tenant lock.");
    REG.embed_before_lock.render(&mut out, "dmem_embed_seconds", "phase=\"before_lock\"");
    REG.embed_under_lock.render(&mut out, "dmem_embed_seconds", "phase=\"under_lock\"");
    if let Some((save, query)) = g.embed_permits {
        head(&mut out, "dmem_embed_permits", "gauge", "Embeddings that may run at once, per pool (DM_EMBED_CONCURRENCY, DM_EMBED_QUERY_CONCURRENCY).");
        let _ = writeln!(out, "dmem_embed_permits{{pool=\"save\"}} {save}");
        let _ = writeln!(out, "dmem_embed_permits{{pool=\"query\"}} {query}");
    }
    head(&mut out, "dmem_embed_permit_wait_seconds", "histogram", "Time a request waited for an embedding permit, per pool.");
    REG.permit_wait_save.render(&mut out, "dmem_embed_permit_wait_seconds", "pool=\"save\"");
    REG.permit_wait_query.render(&mut out, "dmem_embed_permit_wait_seconds", "pool=\"query\"");
    head(&mut out, "dmem_tenant_lock_wait_seconds", "histogram", "Time a request waited for its tenant's lock.");
    REG.lock_wait.render(&mut out, "dmem_tenant_lock_wait_seconds", "");
    head(&mut out, "dmem_tenant_lock_hold_seconds", "histogram", "Time a request held its tenant's lock.");
    REG.lock_hold.render(&mut out, "dmem_tenant_lock_hold_seconds", "");
    head(&mut out, "dmem_zvec_failures_total", "counter", "Vector-index operations that failed.");
    for (i, name) in ZVEC_OPS.iter().enumerate() {
        let _ = writeln!(out, "dmem_zvec_failures_total{{op=\"{name}\"}} {}", REG.zvec_failures[i].load(Ordering::Relaxed));
    }
    let routes = REG.routes.lock().unwrap_or_else(|p| p.into_inner());
    head(&mut out, "dmem_request_duration_seconds", "histogram", "Request latency by route.");
    for (route, (h, _)) in routes.iter() {
        h.render(&mut out, "dmem_request_duration_seconds", &format!("route=\"{route}\""));
    }
    head(&mut out, "dmem_responses_total", "counter", "Responses by route and status class.");
    for (route, (_, classes)) in routes.iter() {
        for (class, n) in classes {
            let _ = writeln!(out, "dmem_responses_total{{route=\"{route}\",class=\"{class}\"}} {n}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_buckets_are_cumulative_and_sum_in_seconds() {
        let h = Histogram::default();
        h.observe(Duration::from_millis(3));
        h.observe(Duration::from_millis(700));
        h.observe(Duration::from_secs(500));
        let mut out = String::new();
        h.render(&mut out, "x", "a=\"b\"");
        assert!(out.contains("x_bucket{a=\"b\",le=\"0.001\"} 0\n"), "{out}");
        assert!(out.contains("x_bucket{a=\"b\",le=\"0.005\"} 1\n"), "{out}");
        assert!(out.contains("x_bucket{a=\"b\",le=\"1\"} 2\n"), "{out}");
        assert!(out.contains("x_bucket{a=\"b\",le=\"120\"} 2\n"), "{out}");
        assert!(out.contains("x_bucket{a=\"b\",le=\"+Inf\"} 3\n"), "{out}");
        assert!(out.contains("x_count{a=\"b\"} 3\n"), "{out}");
        assert!(out.contains("x_sum{a=\"b\"} 500.703"), "{out}");
        let mut bare = String::new();
        h.render(&mut bare, "y", "");
        assert!(bare.contains("y_bucket{le=\"+Inf\"} 3\n") && bare.contains("y_count 3\n"), "{bare}");
    }

    #[test]
    fn render_names_the_embedder_and_counts_zvec_failures() {
        let before = zvec_failures(ZvecOp::Search);
        zvec_failure(ZvecOp::Search);
        assert_eq!(zvec_failures(ZvecOp::Search), before + 1);
        request("/recall", 200, Duration::from_millis(12));
        request("/recall", 500, Duration::from_millis(12));
        let out = render(&Gauges {
            ready: false,
            shutting_down: false,
            in_flight: 2,
            tenants_open: 1,
            tenants_without_vectors: 0,
            embedder: Some("hash"),
            embed_permits: Some((2, 3)),
        });
        assert!(out.contains("dmem_embedder_info{name=\"hash\",placeholder=\"true\"} 1"), "{out}");
        assert!(out.contains("dmem_ready 0\n") && out.contains("dmem_requests_in_flight 2\n"), "{out}");
        assert!(out.contains(&format!("dmem_zvec_failures_total{{op=\"search\"}} {}", before + 1)), "{out}");
        assert!(out.contains("dmem_responses_total{route=\"/recall\",class=\"5xx\"}"), "{out}");
        assert!(out.contains("# TYPE dmem_request_duration_seconds histogram"), "{out}");
        assert!(out.contains("dmem_embed_permits{pool=\"save\"} 2\n") && out.contains("dmem_embed_permits{pool=\"query\"} 3\n"), "{out}");
        assert!(out.contains("dmem_embed_permit_wait_seconds_count{pool=\"query\"}"), "{out}");
    }
}
