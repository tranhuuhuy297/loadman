//! Turning recorded metrics into a report: terminal summary and JSON.

use std::fmt::Write as _;
use std::io::IsTerminal;

use serde::Serialize;

use crate::collection::Plan;
use crate::engine::RunOutput;
use crate::metrics::{RequestAcc, WindowSummary};
use crate::profile::{ControlPoint, LoadProfile};
use crate::threshold::{Threshold, ThresholdResult};

#[derive(Debug, Clone, Default, Serialize)]
pub struct Totals {
    pub requests: u64,
    pub rps: f64,
    pub avg_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    /// Requests with a transport or script error, in percent.
    pub error_rate: f64,
    pub transport_errors: u64,
    pub script_errors: u64,
    /// Requests whose response status was 4xx/5xx, in percent.
    pub http_error_rate: f64,
    pub http_errors: u64,
    /// Requests with at least one failed `pm.test`, in percent.
    pub assertion_failure_rate: f64,
    pub failed_assertion_requests: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// Whole run only, for thresholds; the report has them at the top level.
    #[serde(skip)]
    pub iterations: u64,
    #[serde(skip)]
    pub dropped_iterations: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestStats {
    pub name: String,
    pub path: String,
    pub method: String,
    #[serde(flatten)]
    pub totals: Totals,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssertionStats {
    pub request: String,
    pub name: String,
    pub passed: u64,
    pub failed: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileInfo {
    pub kind: &'static str,
    pub max_vus: u32,
    pub duration_ms: u64,
    pub vu_hours: f64,
    pub points: Vec<ControlPoint>,
}

/// The open model's settings and what it did (`--rate`).
#[derive(Debug, Clone, Serialize)]
pub struct ArrivalInfo {
    /// Iterations per second at the profile's peak; the profile's points
    /// scale to it.
    pub peak_rate: f64,
    pub max_vus: u32,
    /// The most VUs that existed at once.
    pub peak_vus: u32,
    pub dropped_iterations: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub tool: &'static str,
    pub version: &'static str,
    pub collection: String,
    pub started_at: String,
    pub duration_s: f64,
    pub interrupted: bool,
    pub iterations: u64,
    pub profile: ProfileInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arrival: Option<ArrivalInfo>,
    pub totals: Totals,
    pub requests: Vec<RequestStats>,
    pub assertions: Vec<AssertionStats>,
    pub errors: Vec<(String, u64)>,
    pub script_errors: Vec<(String, u64)>,
    pub status_codes: Vec<(u16, u64)>,
    pub thresholds: Vec<ThresholdResult>,
    pub warnings: Vec<String>,
    pub timeline: Vec<WindowSummary>,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.thresholds.iter().all(|t| t.passed)
    }
}

fn totals(acc: &RequestAcc, secs: f64) -> Totals {
    let n = acc.count;
    let pct = |x: u64| if n == 0 { 0.0 } else { x as f64 * 100.0 / n as f64 };
    let q = |p: f64| if n == 0 { 0.0 } else { acc.hist.value_at_quantile(p) as f64 / 1000.0 };
    Totals {
        requests: n,
        rps: if secs > 0.0 { n as f64 / secs } else { 0.0 },
        avg_ms: if n == 0 { 0.0 } else { acc.sum_us as f64 / n as f64 / 1000.0 },
        min_ms: if n == 0 { 0.0 } else { acc.min_us as f64 / 1000.0 },
        max_ms: acc.max_us as f64 / 1000.0,
        p50_ms: q(0.50),
        p90_ms: q(0.90),
        p95_ms: q(0.95),
        p99_ms: q(0.99),
        error_rate: pct(acc.error_executions),
        transport_errors: acc.transport_errors,
        script_errors: acc.script_errors,
        http_error_rate: pct(acc.http_errors),
        http_errors: acc.http_errors,
        assertion_failure_rate: pct(acc.failed_assertion_executions),
        failed_assertion_requests: acc.failed_assertion_executions,
        bytes_in: acc.bytes_in,
        bytes_out: acc.bytes_out,
        ..Totals::default()
    }
}

fn sorted<K: Clone + Ord>(m: &std::collections::HashMap<K, u64>) -> Vec<(K, u64)> {
    let mut v: Vec<(K, u64)> = m.iter().map(|(k, v)| (k.clone(), *v)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

pub fn build(plan: &Plan, profile: &LoadProfile, out: RunOutput, thresholds: &[Threshold]) -> Report {
    let secs = out.elapsed.as_secs_f64();
    let mut all = totals(&out.aggregate.overall(), secs);
    all.iterations = out.iterations;
    all.dropped_iterations = out.dropped_iterations;
    let requests = plan
        .requests
        .iter()
        .zip(&out.aggregate.requests)
        .map(|(spec, acc)| RequestStats {
            name: spec.name.clone(),
            path: spec.path.clone(),
            method: spec.method.clone(),
            totals: totals(acc, secs),
        })
        .collect();
    let mut assertions: Vec<AssertionStats> = out
        .aggregate
        .assertions
        .iter()
        .map(|((req, name), (p, f))| AssertionStats {
            request: plan.requests[*req].path.clone(),
            name: name.clone(),
            passed: *p,
            failed: *f,
        })
        .collect();
    assertions.sort_by(|a, b| a.request.cmp(&b.request).then_with(|| a.name.cmp(&b.name)));
    let mut status_codes = sorted(&out.aggregate.status);
    status_codes.sort_by_key(|(code, _)| *code);
    Report {
        tool: "loadman",
        version: env!("CARGO_PKG_VERSION"),
        collection: plan.name.clone(),
        started_at: out.started_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        duration_s: secs,
        interrupted: out.interrupted,
        iterations: out.iterations,
        profile: ProfileInfo {
            kind: profile.kind.label(),
            max_vus: profile.max_vus(),
            duration_ms: profile.duration_ms,
            vu_hours: profile.vu_hours(),
            points: profile.points.clone(),
        },
        arrival: out.arrival.map(|a| ArrivalInfo {
            peak_rate: a.peak_rate,
            max_vus: a.max_vus,
            peak_vus: out.peak_vus,
            dropped_iterations: out.dropped_iterations,
        }),
        thresholds: thresholds.iter().map(|t| t.evaluate(&all)).collect(),
        totals: all,
        requests,
        assertions,
        errors: sorted(&out.aggregate.errors),
        script_errors: sorted(&out.aggregate.script_messages),
        status_codes,
        warnings: out.warnings,
        timeline: out.timeline,
    }
}

// ---------------------------------------------------------------------------
// Terminal output
// ---------------------------------------------------------------------------

pub struct Style {
    color: bool,
}

impl Style {
    pub fn detect() -> Style {
        Style { color: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() }
    }
    fn paint(&self, code: &str, s: &str) -> String {
        if self.color { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }
    pub fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    pub fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
    pub fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }
    pub fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }
}

pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

pub fn ms(v: f64) -> String {
    if v >= 1000.0 {
        format!("{:.2}s", v / 1000.0)
    } else if v >= 100.0 {
        format!("{v:.0}ms")
    } else if v >= 1.0 {
        format!("{v:.1}ms")
    } else {
        format!("{v:.2}ms")
    }
}

fn pad(s: &str, w: usize) -> String {
    let len = s.chars().count();
    if len >= w { s.chars().take(w).collect() } else { format!("{s}{}", " ".repeat(w - len)) }
}

fn rpad(s: &str, w: usize) -> String {
    let len = s.chars().count();
    if len >= w { s.to_string() } else { format!("{}{s}", " ".repeat(w - len)) }
}

/// `12.5` → `12.5`, `100.0` → `100`.
pub fn rate(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// "up to 50 VUs", or "up to 200 it/s · at most 1000 VUs" with `--rate`.
pub fn load_label(max_vus: u32, arrival: Option<(f64, u32)>) -> String {
    match arrival {
        Some((peak, max)) => format!("up to {} it/s · at most {max} VUs", rate(peak)),
        None => format!("up to {max_vus} VUs"),
    }
}

pub fn render_terminal(r: &Report, st: &Style) -> String {
    let t = &r.totals;
    let mut o = String::new();
    let _ = writeln!(
        o,
        "\n{}  {} · {} profile · {} · {:.1}s{}",
        st.bold("loadman"),
        r.collection,
        r.profile.kind,
        load_label(r.profile.max_vus, r.arrival.as_ref().map(|a| (a.peak_rate, a.max_vus))),
        r.duration_s,
        if r.interrupted { st.yellow(" · interrupted") } else { String::new() }
    );
    let err_txt = format!("{:.2}% ({})", t.error_rate, thousands(t.transport_errors + t.script_errors));
    let _ = writeln!(
        o,
        "\n  {}  {} ({:.1}/s)   {}  {}",
        st.dim("requests "),
        st.bold(&thousands(t.requests)),
        t.rps,
        st.dim("errors"),
        if t.error_rate > 0.0 { st.red(&err_txt) } else { st.green(&err_txt) }
    );
    let _ = writeln!(
        o,
        "  {}  avg {} · p90 {} · p95 {} · p99 {} · max {}",
        st.dim("latency  "),
        st.bold(&ms(t.avg_ms)),
        ms(t.p90_ms),
        st.bold(&ms(t.p95_ms)),
        ms(t.p99_ms),
        ms(t.max_ms)
    );
    if !r.assertions.is_empty() {
        let passed: u64 = r.assertions.iter().map(|a| a.passed).sum();
        let failed: u64 = r.assertions.iter().map(|a| a.failed).sum();
        let f = format!("{} failed", thousands(failed));
        let _ = writeln!(
            o,
            "  {}  {} passed · {} ({:.2}% of requests)",
            st.dim("tests    "),
            thousands(passed),
            if failed > 0 { st.red(&f) } else { st.green(&f) },
            t.assertion_failure_rate
        );
    }
    let _ = writeln!(
        o,
        "  {}  {} received · {} sent · {} iterations · HTTP 4xx/5xx {:.2}%",
        st.dim("traffic  "),
        bytes(t.bytes_in),
        bytes(t.bytes_out),
        thousands(r.iterations),
        t.http_error_rate
    );
    if let Some(a) = &r.arrival {
        let dropped = format!("{} dropped", thousands(a.dropped_iterations));
        let _ = writeln!(
            o,
            "  {}  {:.1} iterations/s · {} · {} VU{} started",
            st.dim("arrivals "),
            if r.duration_s > 0.0 { (r.iterations + a.dropped_iterations) as f64 / r.duration_s } else { 0.0 },
            if a.dropped_iterations > 0 { st.yellow(&dropped) } else { st.green(&dropped) },
            a.peak_vus,
            if a.peak_vus == 1 { "" } else { "s" }
        );
    }

    let _ = writeln!(
        o,
        "\n  {}",
        st.dim(&format!(
            "{} {} {} {} {} {} {} {}",
            pad("REQUEST", 34),
            rpad("COUNT", 9),
            rpad("RPS", 8),
            rpad("AVG", 8),
            rpad("P90", 8),
            rpad("P95", 8),
            rpad("P99", 8),
            rpad("ERR%", 7)
        ))
    );
    for q in &r.requests {
        let name = format!("{} {}", q.method, q.path);
        let e = format!("{:.2}", q.totals.error_rate);
        let _ = writeln!(
            o,
            "  {} {} {} {} {} {} {} {}",
            pad(&name, 34),
            rpad(&thousands(q.totals.requests), 9),
            rpad(&format!("{:.1}", q.totals.rps), 8),
            rpad(&ms(q.totals.avg_ms), 8),
            rpad(&ms(q.totals.p90_ms), 8),
            rpad(&ms(q.totals.p95_ms), 8),
            rpad(&ms(q.totals.p99_ms), 8),
            if q.totals.error_rate > 0.0 { st.red(&rpad(&e, 7)) } else { rpad(&e, 7) }
        );
    }

    let failing: Vec<_> = r.assertions.iter().filter(|a| a.failed > 0).collect();
    if !failing.is_empty() {
        let _ = writeln!(o, "\n  {}", st.dim("FAILED TESTS"));
        for a in failing.iter().take(10) {
            let _ = writeln!(
                o,
                "  {} {} › {} ({} of {})",
                st.red("✗"),
                a.request,
                a.name,
                thousands(a.failed),
                thousands(a.failed + a.passed)
            );
        }
    }
    if !r.errors.is_empty() || !r.script_errors.is_empty() {
        let _ = writeln!(o, "\n  {}", st.dim("ERRORS"));
        for (name, n) in r.errors.iter().take(8) {
            let _ = writeln!(o, "  {} {name} ×{}", st.red("•"), thousands(*n));
        }
        for (msg, n) in r.script_errors.iter().take(5) {
            let _ = writeln!(o, "  {} {msg} ×{}", st.red("•"), thousands(*n));
        }
    }
    if !r.status_codes.is_empty() {
        let codes: Vec<String> = r.status_codes.iter().map(|(c, n)| format!("{c} ×{}", thousands(*n))).collect();
        let _ = writeln!(o, "\n  {} {}", st.dim("status  "), codes.join(" · "));
    }
    if !r.thresholds.is_empty() {
        let _ = writeln!(o, "\n  {}", st.dim("THRESHOLDS"));
        for th in &r.thresholds {
            let mark = if th.passed { st.green("✓") } else { st.red("✗") };
            let _ = writeln!(o, "  {mark} {}  {}", th.expr, st.dim(&format!("actual {}", th.actual_text())));
        }
    }
    for w in &r.warnings {
        let _ = writeln!(o, "  {} {w}", st.yellow("!"));
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(thousands(12), "12");
        assert_eq!(bytes(2048), "2.0 KB");
        assert_eq!(ms(12.34), "12.3ms");
        assert_eq!(ms(1500.0), "1.50s");
        assert_eq!(ms(0.634), "0.63ms");
    }
}
