//! Low-overhead metric recording.
//!
//! Virtual users record into one of N mutex-protected shards (VU index modulo
//! N), so concurrent VUs rarely touch the same lock. Latencies go into HDR
//! histograms: exact counts, percentiles within 0.1 % for whole-run and
//! per-request figures and within 1 % for the per-second timeline. Per-second
//! windows are drained into the timeline once a second and their histograms
//! are dropped, so memory stays flat no matter how long the test runs.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use hdrhistogram::Histogram;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Latencies are recorded in microseconds and clamped to 10 minutes.
const MAX_US: u64 = 600_000_000;
const MAX_SCRIPT_MESSAGES: usize = 50;

fn hist(sigfig: u8) -> Histogram<u32> {
    Histogram::new_with_bounds(1, MAX_US, sigfig).expect("valid histogram bounds")
}

/// Histograms on the wire (distributed runs): V2 deflate encoding, base64.
mod hist_b64 {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use hdrhistogram::Histogram;
    use hdrhistogram::serialization::{Deserializer, Serializer as _, V2DeflateSerializer};
    use serde::de::Error as _;
    use serde::{Deserialize as _, ser::Error as _};

    pub fn serialize<S: serde::Serializer>(h: &Histogram<u32>, s: S) -> Result<S::Ok, S::Error> {
        let mut buf = Vec::new();
        V2DeflateSerializer::new().serialize(h, &mut buf).map_err(|e| S::Error::custom(format!("{e:?}")))?;
        s.serialize_str(&STANDARD.encode(buf))
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Histogram<u32>, D::Error> {
        let bytes = STANDARD.decode(String::deserialize(d)?).map_err(D::Error::custom)?;
        Deserializer::new().deserialize(&mut bytes.as_slice()).map_err(|e| D::Error::custom(format!("{e:?}")))
    }
}

/// One finished request execution.
pub struct Sample<'a> {
    pub request: usize,
    pub latency_us: u64,
    /// HTTP status, 0 when the request failed before a response arrived.
    pub status: u16,
    pub transport_error: Option<&'static str>,
    pub script_error: Option<&'a str>,
    pub assertions: &'a [(String, bool)],
    pub bytes_in: u64,
    pub bytes_out: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct WindowAcc {
    #[serde(with = "hist_b64")]
    pub hist: Histogram<u32>,
    pub count: u64,
    pub sum_us: u64,
    pub errors: u64,
    pub failed_assertions: u64,
    pub http_errors: u64,
    pub bytes_in: u64,
}

impl WindowAcc {
    pub fn new() -> Self {
        WindowAcc { hist: hist(2), count: 0, sum_us: 0, errors: 0, failed_assertions: 0, http_errors: 0, bytes_in: 0 }
    }

    pub fn merge(&mut self, o: &WindowAcc) {
        let _ = self.hist.add(&o.hist);
        self.count += o.count;
        self.sum_us += o.sum_us;
        self.errors += o.errors;
        self.failed_assertions += o.failed_assertions;
        self.http_errors += o.http_errors;
        self.bytes_in += o.bytes_in;
    }
}

impl Default for WindowAcc {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RequestAcc {
    #[serde(with = "hist_b64")]
    pub hist: Histogram<u32>,
    pub count: u64,
    pub sum_us: u64,
    pub min_us: u64,
    pub max_us: u64,
    pub transport_errors: u64,
    pub script_errors: u64,
    /// Executions with a transport or script error (Postman's error rate numerator).
    pub error_executions: u64,
    pub failed_assertion_executions: u64,
    pub http_errors: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

impl RequestAcc {
    pub fn new() -> Self {
        RequestAcc {
            hist: hist(3),
            count: 0,
            sum_us: 0,
            min_us: u64::MAX,
            max_us: 0,
            transport_errors: 0,
            script_errors: 0,
            error_executions: 0,
            failed_assertion_executions: 0,
            http_errors: 0,
            bytes_in: 0,
            bytes_out: 0,
        }
    }

    pub fn merge(&mut self, o: &RequestAcc) {
        let _ = self.hist.add(&o.hist);
        self.count += o.count;
        self.sum_us += o.sum_us;
        self.min_us = self.min_us.min(o.min_us);
        self.max_us = self.max_us.max(o.max_us);
        self.transport_errors += o.transport_errors;
        self.script_errors += o.script_errors;
        self.error_executions += o.error_executions;
        self.failed_assertion_executions += o.failed_assertion_executions;
        self.http_errors += o.http_errors;
        self.bytes_in += o.bytes_in;
        self.bytes_out += o.bytes_out;
    }
}

impl Default for RequestAcc {
    fn default() -> Self {
        Self::new()
    }
}

struct Shard {
    windows: Vec<(u64, WindowAcc)>,
    requests: Vec<RequestAcc>,
    errors: HashMap<&'static str, u64>,
    status: HashMap<u16, u64>,
    /// Per request: assertion name -> (passed, failed).
    assertions: Vec<HashMap<String, (u64, u64)>>,
    script_messages: HashMap<String, u64>,
}

pub struct Recorder {
    start: Instant,
    shards: Box<[Mutex<Shard>]>,
    /// Live counters for the progress line.
    pub total: AtomicU64,
    pub total_errors: AtomicU64,
    pub active_vus: AtomicU32,
}

impl Recorder {
    pub fn new(start: Instant, requests: usize, shards: usize) -> Self {
        let shards = (0..shards.max(1))
            .map(|_| {
                Mutex::new(Shard {
                    windows: Vec::new(),
                    requests: (0..requests).map(|_| RequestAcc::new()).collect(),
                    errors: HashMap::new(),
                    status: HashMap::new(),
                    assertions: (0..requests).map(|_| HashMap::new()).collect(),
                    script_messages: HashMap::new(),
                })
            })
            .collect();
        Recorder {
            start,
            shards,
            total: AtomicU64::new(0),
            total_errors: AtomicU64::new(0),
            active_vus: AtomicU32::new(0),
        }
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    pub fn record(&self, shard: usize, s: &Sample) {
        let sec = self.start.elapsed().as_secs();
        let lat = s.latency_us.clamp(1, MAX_US);
        let is_error = s.transport_error.is_some() || s.script_error.is_some();
        let failed = s.assertions.iter().any(|(_, ok)| !ok);
        let http_error = s.status >= 400;

        let mut g = self.shards[shard % self.shards.len()].lock();
        let w = match g.windows.iter().rposition(|(t, _)| *t == sec) {
            Some(i) => &mut g.windows[i].1,
            None => {
                g.windows.push((sec, WindowAcc::new()));
                &mut g.windows.last_mut().unwrap().1
            }
        };
        w.hist.saturating_record(lat);
        w.count += 1;
        w.sum_us += lat;
        w.errors += is_error as u64;
        w.failed_assertions += failed as u64;
        w.http_errors += http_error as u64;
        w.bytes_in += s.bytes_in;

        let r = &mut g.requests[s.request];
        r.hist.saturating_record(lat);
        r.count += 1;
        r.sum_us += lat;
        r.min_us = r.min_us.min(lat);
        r.max_us = r.max_us.max(lat);
        r.transport_errors += s.transport_error.is_some() as u64;
        r.script_errors += s.script_error.is_some() as u64;
        r.error_executions += is_error as u64;
        r.failed_assertion_executions += failed as u64;
        r.http_errors += http_error as u64;
        r.bytes_in += s.bytes_in;
        r.bytes_out += s.bytes_out;

        if let Some(e) = s.transport_error {
            *g.errors.entry(e).or_default() += 1;
        }
        if s.status > 0 {
            *g.status.entry(s.status).or_default() += 1;
        }
        if let Some(msg) = s.script_error {
            let full = g.script_messages.len() >= MAX_SCRIPT_MESSAGES;
            match g.script_messages.get_mut(msg) {
                Some(n) => *n += 1,
                None if !full => {
                    g.script_messages.insert(msg.to_string(), 1);
                }
                None => *g.script_messages.entry("(other script errors)".into()).or_default() += 1,
            }
        }
        let per_test = &mut g.assertions[s.request];
        for (name, ok) in s.assertions {
            if !per_test.contains_key(name.as_str()) {
                per_test.insert(name.clone(), (0, 0));
            }
            let e = per_test.get_mut(name.as_str()).unwrap();
            if *ok { e.0 += 1 } else { e.1 += 1 }
        }
        drop(g);
        self.total.fetch_add(1, Ordering::Relaxed);
        if is_error {
            self.total_errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Move every per-second window older than `before_sec` out of the shards.
    fn drain_windows(&self, before_sec: u64, into: &mut BTreeMap<u64, WindowAcc>) {
        for shard in self.shards.iter() {
            let mut g = shard.lock();
            let mut i = 0;
            while i < g.windows.len() {
                if g.windows[i].0 < before_sec {
                    let (t, w) = g.windows.swap_remove(i);
                    into.entry(t).or_default().merge(&w);
                } else {
                    i += 1;
                }
            }
        }
    }

    /// Merge all shards into whole-run totals.
    pub fn aggregate(&self) -> Aggregate {
        let mut out = Aggregate::default();
        for shard in self.shards.iter() {
            let g = shard.lock();
            if out.requests.is_empty() {
                out.requests = g.requests.iter().map(|_| RequestAcc::new()).collect();
            }
            for (acc, r) in out.requests.iter_mut().zip(&g.requests) {
                acc.merge(r);
            }
            for (k, v) in &g.errors {
                *out.errors.entry(k.to_string()).or_default() += v;
            }
            for (k, v) in &g.status {
                *out.status.entry(*k).or_default() += v;
            }
            for (req, tests) in g.assertions.iter().enumerate() {
                for (name, v) in tests {
                    let e = out.assertions.entry((req, name.clone())).or_default();
                    e.0 += v.0;
                    e.1 += v.1;
                }
            }
            for (k, v) in &g.script_messages {
                *out.script_messages.entry(k.clone()).or_default() += v;
            }
        }
        out
    }
}

#[derive(Default)]
pub struct Aggregate {
    pub requests: Vec<RequestAcc>,
    pub errors: HashMap<String, u64>,
    pub status: HashMap<u16, u64>,
    pub assertions: HashMap<(usize, String), (u64, u64)>,
    pub script_messages: HashMap<String, u64>,
}

impl Aggregate {
    pub fn overall(&self) -> RequestAcc {
        let mut all = RequestAcc::new();
        for r in &self.requests {
            all.merge(r);
        }
        all
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowSummary {
    /// Seconds since the start of the run.
    pub t: u64,
    pub vus: u32,
    pub requests: u64,
    pub rps: f64,
    pub avg_ms: f64,
    pub p90_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    /// Transport + script errors, percent of requests in the window.
    pub error_rate: f64,
    pub http_error_rate: f64,
    pub bytes_in: u64,
}

fn ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

impl WindowSummary {
    pub fn from_acc(t: u64, vus: u32, w: &WindowAcc) -> Self {
        let pct = |n: u64| if w.count == 0 { 0.0 } else { n as f64 * 100.0 / w.count as f64 };
        let q = |p: f64| if w.count == 0 { 0.0 } else { ms(w.hist.value_at_quantile(p)) };
        WindowSummary {
            t,
            vus,
            requests: w.count,
            rps: w.count as f64,
            avg_ms: if w.count == 0 { 0.0 } else { w.sum_us as f64 / w.count as f64 / 1000.0 },
            p90_ms: q(0.90),
            p95_ms: q(0.95),
            p99_ms: q(0.99),
            max_ms: if w.count == 0 { 0.0 } else { ms(w.hist.max()) },
            error_rate: pct(w.errors),
            http_error_rate: pct(w.http_errors),
            bytes_in: w.bytes_in,
        }
    }
}

/// Per-second history built while the test runs.
#[derive(Default)]
pub struct Timeline {
    pending: BTreeMap<u64, WindowAcc>,
    vus: BTreeMap<u64, u32>,
    pub points: Vec<WindowSummary>,
    /// Also keep each second's accumulator, JSON-encoded so its histogram
    /// stays small, for merging distributed runs exactly.
    pub keep: bool,
    pub kept: Vec<(u64, u32, String)>,
}

impl Timeline {
    pub fn set_vus(&mut self, sec: u64, vus: u32) {
        let e = self.vus.entry(sec).or_insert(0);
        *e = (*e).max(vus);
    }

    fn vus_at(&self, sec: u64) -> u32 {
        self.vus.range(..=sec).next_back().map(|(_, v)| *v).unwrap_or(0)
    }

    /// Called once per second with the number of whole seconds elapsed.
    /// Returns the most recent complete second for the live progress line.
    pub fn tick(&mut self, rec: &Recorder, now_sec: u64) -> Option<WindowSummary> {
        rec.drain_windows(now_sec, &mut self.pending);
        // Keep the last complete second pending one extra tick so stragglers
        // recorded right at the boundary still land in the right window.
        self.finalize_before(now_sec.saturating_sub(1));
        let last = now_sec.checked_sub(1)?;
        self.pending.get(&last).map(|w| WindowSummary::from_acc(last, self.vus_at(last), w))
    }

    pub fn finish(&mut self, rec: &Recorder, end_sec: u64) {
        rec.drain_windows(u64::MAX, &mut self.pending);
        self.finalize_before(end_sec.max(self.pending.keys().next_back().map_or(0, |k| k + 1)));
    }

    fn finalize_before(&mut self, before: u64) {
        let mut next = self.points.last().map_or(0, |p| p.t + 1);
        while next < before {
            let w = self.pending.remove(&next).unwrap_or_default();
            self.points.push(WindowSummary::from_acc(next, self.vus_at(next), &w));
            if self.keep {
                self.kept.push((next, self.vus_at(next), serde_json::to_string(&w).unwrap_or_default()));
            }
            next += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(req: usize, lat_ms: u64, status: u16) -> Sample<'static> {
        Sample {
            request: req,
            latency_us: lat_ms * 1000,
            status,
            transport_error: if status == 0 { Some("ECONNREFUSED") } else { None },
            script_error: None,
            assertions: &[],
            bytes_in: 10,
            bytes_out: 5,
        }
    }

    #[test]
    fn aggregates_across_shards() {
        let rec = Recorder::new(Instant::now(), 2, 4);
        for i in 0..100u64 {
            rec.record(i as usize, &sample((i % 2) as usize, i + 1, 200));
        }
        rec.record(1, &sample(0, 5, 0));
        rec.record(2, &sample(1, 5, 503));
        let agg = rec.aggregate();
        let all = agg.overall();
        assert_eq!(all.count, 102);
        assert_eq!(all.transport_errors, 1);
        assert_eq!(all.http_errors, 1);
        assert_eq!(agg.errors["ECONNREFUSED"], 1);
        assert_eq!(agg.status[&200], 100);
        let p50 = all.hist.value_at_quantile(0.5) as f64 / 1000.0;
        assert!((48.0..=52.0).contains(&p50), "p50 {p50}");
        assert_eq!(all.max_us, 100_000);
    }

    #[test]
    fn timeline_fills_gaps() {
        let rec = Recorder::new(Instant::now(), 1, 2);
        rec.record(0, &sample(0, 10, 200));
        let mut tl = Timeline::default();
        tl.set_vus(0, 3);
        tl.tick(&rec, 1);
        tl.finish(&rec, 3);
        assert_eq!(tl.points.len(), 3);
        assert_eq!(tl.points[0].requests, 1);
        assert_eq!(tl.points[2].requests, 0);
        assert_eq!(tl.points[2].vus, 3);
    }
}
