//! The load generator: schedules virtual users along a load profile and runs
//! the collection's requests in a closed loop.
//!
//! Model (matches the behaviour documented in docs/RESEARCH.md): every tick
//! the target VU count is read from the profile; new VUs get the next index,
//! surplus VUs are stopped newest first. Each VU runs the selected requests
//! in order, then starts over, until it is stopped. A stopped VU finishes the
//! request it is executing, which is still recorded.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine as _;
use bytes::Bytes;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, Url};
use tokio::task::JoinHandle;

use crate::collection::{AuthSpec, BodySpec, FormPart, Plan, RequestSpec};
use crate::data::DataSet;
use crate::metrics::{Aggregate, Recorder, Sample, Timeline, WindowSummary};
use crate::profile::LoadProfile;
use crate::script::{ExecContext, ExecResponse, HeaderOp, Phase, ScriptPool};
use crate::template::{Lookup, NoVars, Template};
use crate::vars::{BaseVars, VuVars};

pub struct RunConfig {
    pub plan: Plan,
    pub profile: LoadProfile,
    pub base: BaseVars,
    pub overrides: HashMap<String, String>,
    pub data: Option<DataSet>,
    pub timeout: Duration,
    pub script_timeout: Duration,
    pub insecure: bool,
    pub follow_redirects: bool,
    pub think_time: Option<(Duration, Duration)>,
    pub tick: Duration,
    pub scripts: bool,
    pub script_threads: usize,
    /// HTTP worker threads (default: one per CPU core).
    pub workers: Option<usize>,
    pub verbose: bool,
}

pub struct RunOutput {
    pub aggregate: Aggregate,
    pub timeline: Vec<WindowSummary>,
    pub elapsed: Duration,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub interrupted: bool,
    pub iterations: u64,
    pub warnings: Vec<String>,
}

/// Snapshot passed to the progress callback once per tick.
pub struct Progress<'a> {
    pub elapsed: Duration,
    pub duration: Duration,
    pub vus: u32,
    pub total_requests: u64,
    pub total_errors: u64,
    pub last_second: Option<&'a WindowSummary>,
}

// ---------------------------------------------------------------------------
// Request preparation
// ---------------------------------------------------------------------------

struct Prepared {
    spec: RequestSpec,
    method: Method,
    static_url: Option<Url>,
    static_headers: Option<HeaderMap>,
    static_body: Option<Option<(Bytes, Option<String>)>>,
    pre_ids: Option<Arc<str>>,
    test_ids: Option<Arc<str>>,
    boundary: String,
}

fn ids_json(ids: &[usize]) -> Option<Arc<str>> {
    (!ids.is_empty()).then(|| serde_json::to_string(ids).unwrap().into())
}

fn tpl_static(t: &Template) -> bool {
    t.as_static().is_some()
}

impl Prepared {
    fn new(spec: RequestSpec, scripts: bool) -> Result<Self> {
        let method = Method::from_bytes(spec.method.as_bytes())
            .with_context(|| format!("`{}`: invalid HTTP method {}", spec.name, spec.method))?;
        let auth_static = match &spec.auth {
            AuthSpec::None => true,
            AuthSpec::Bearer(t) => tpl_static(t),
            AuthSpec::Basic { user, pass } => tpl_static(user) && tpl_static(pass),
            AuthSpec::ApiKey { key, value, .. } => tpl_static(key) && tpl_static(value),
        };
        let query_auth = matches!(spec.auth, AuthSpec::ApiKey { in_query: true, .. });
        let body_static = match &spec.body {
            BodySpec::None | BodySpec::Bytes { .. } => true,
            BodySpec::Raw { text, .. } => tpl_static(text),
            BodySpec::UrlEncoded(kv) => kv.iter().all(|(k, v)| tpl_static(k) && tpl_static(v)),
            BodySpec::Graphql { query, variables } => tpl_static(query) && tpl_static(variables),
            BodySpec::Multipart(parts) => parts.iter().all(|p| match p {
                FormPart::Text { name, value, .. } => tpl_static(name) && tpl_static(value),
                FormPart::File { name, .. } => tpl_static(name),
            }),
        };
        let headers_static = spec.headers.iter().all(|(k, v)| tpl_static(k) && tpl_static(v));
        let mut p = Prepared {
            pre_ids: if scripts { ids_json(&spec.pre_scripts) } else { None },
            test_ids: if scripts { ids_json(&spec.test_scripts) } else { None },
            boundary: format!("----loadman{:016x}", fastrand::u64(..)),
            spec,
            method,
            static_url: None,
            static_headers: None,
            static_body: None,
        };
        if tpl_static(&p.spec.url) && !(query_auth && !auth_static) {
            p.static_url = p.url(&NoVars).ok();
        }
        if body_static {
            p.static_body = Some(p.body(&NoVars));
        }
        if headers_static && auth_static && body_static {
            p.static_headers = Some(p.headers(&NoVars));
        }
        Ok(p)
    }

    fn url(&self, vars: &dyn Lookup) -> Result<Url, &'static str> {
        if let Some(u) = &self.static_url {
            return Ok(u.clone());
        }
        let raw = self.spec.url.render(vars);
        let raw = raw.trim();
        let mut url = if raw.contains("://") { Url::parse(raw) } else { Url::parse(&format!("http://{raw}")) }
            .map_err(|_| "EINVALIDURL")?;
        if let AuthSpec::ApiKey { key, value, in_query: true } = &self.spec.auth {
            url.query_pairs_mut().append_pair(&key.render(vars), &value.render(vars));
        }
        Ok(url)
    }

    fn headers(&self, vars: &dyn Lookup) -> HeaderMap {
        if let Some(h) = &self.static_headers {
            return h.clone();
        }
        let mut map = HeaderMap::with_capacity(self.spec.headers.len() + 2);
        for (k, v) in &self.spec.headers {
            let (k, v) = (k.render(vars), v.render(vars));
            if let (Ok(name), Ok(value)) =
                (HeaderName::from_bytes(k.trim().as_bytes()), HeaderValue::from_str(v.trim()))
            {
                map.append(name, value);
            }
        }
        let set = |map: &mut HeaderMap, name: HeaderName, value: String| {
            if !map.contains_key(&name)
                && let Ok(v) = HeaderValue::from_str(&value)
            {
                map.insert(name, v);
            }
        };
        match &self.spec.auth {
            AuthSpec::None => {}
            AuthSpec::Bearer(t) => set(&mut map, AUTHORIZATION, format!("Bearer {}", t.render(vars))),
            AuthSpec::Basic { user, pass } => {
                let raw = format!("{}:{}", user.render(vars), pass.render(vars));
                set(
                    &mut map,
                    AUTHORIZATION,
                    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw)),
                );
            }
            AuthSpec::ApiKey { key, value, in_query: false } => {
                if let Ok(name) = HeaderName::from_bytes(key.render(vars).as_bytes()) {
                    set(&mut map, name, value.render(vars));
                }
            }
            AuthSpec::ApiKey { .. } => {}
        }
        let content_type = match &self.static_body {
            Some(Some((_, ct))) => ct.clone(),
            Some(None) => None,
            None => self.body_content_type(),
        };
        if let Some(ct) = content_type {
            set(&mut map, CONTENT_TYPE, ct);
        }
        map
    }

    fn body_content_type(&self) -> Option<String> {
        match &self.spec.body {
            BodySpec::None => None,
            BodySpec::Raw { content_type, .. } => content_type.map(String::from),
            BodySpec::UrlEncoded(_) => Some("application/x-www-form-urlencoded".into()),
            BodySpec::Multipart(_) => Some(format!("multipart/form-data; boundary={}", self.boundary)),
            BodySpec::Bytes { content_type, .. } => content_type.clone(),
            BodySpec::Graphql { .. } => Some("application/json".into()),
        }
    }

    fn body(&self, vars: &dyn Lookup) -> Option<(Bytes, Option<String>)> {
        if let Some(b) = &self.static_body {
            return b.clone();
        }
        let ct = self.body_content_type();
        let bytes: Bytes = match &self.spec.body {
            BodySpec::None => return None,
            BodySpec::Raw { text, .. } => text.render(vars).into(),
            BodySpec::Bytes { data, .. } => data.clone(),
            BodySpec::UrlEncoded(kv) => kv
                .iter()
                .map(|(k, v)| format!("{}={}", form_encode(&k.render(vars)), form_encode(&v.render(vars))))
                .collect::<Vec<_>>()
                .join("&")
                .into(),
            BodySpec::Graphql { query, variables } => {
                let vars_text = variables.render(vars);
                let parsed: serde_json::Value = if vars_text.trim().is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(&vars_text).unwrap_or_else(|_| serde_json::json!({}))
                };
                serde_json::to_vec(&serde_json::json!({ "query": query.render(vars), "variables": parsed }))
                    .unwrap_or_default()
                    .into()
            }
            BodySpec::Multipart(parts) => {
                let mut out = Vec::new();
                for part in parts {
                    out.extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
                    match part {
                        FormPart::Text { name, value, content_type } => {
                            out.extend_from_slice(
                                format!("Content-Disposition: form-data; name=\"{}\"\r\n", name.render(vars))
                                    .as_bytes(),
                            );
                            if let Some(ct) = content_type {
                                out.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
                            }
                            out.extend_from_slice(b"\r\n");
                            out.extend_from_slice(value.render(vars).as_bytes());
                        }
                        FormPart::File { name, file_name, data, content_type } => {
                            out.extend_from_slice(
                                format!(
                                    "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
                                    name.render(vars),
                                    file_name,
                                    content_type.as_deref().unwrap_or("application/octet-stream")
                                )
                                .as_bytes(),
                            );
                            out.extend_from_slice(data);
                        }
                    }
                    out.extend_from_slice(b"\r\n");
                }
                out.extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
                out.into()
            }
        };
        Some((bytes, ct))
    }
}

/// `application/x-www-form-urlencoded` encoding.
fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn header_pairs(map: &HeaderMap) -> Vec<(String, String)> {
    map.iter().map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned())).collect()
}

fn apply_header_ops(map: &mut HeaderMap, ops: &[HeaderOp]) {
    for op in ops {
        match op {
            HeaderOp::Set(k, v) | HeaderOp::Add(k, v) => {
                if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
                    if matches!(op, HeaderOp::Set(..)) {
                        map.insert(name, value);
                    } else {
                        map.append(name, value);
                    }
                }
            }
            HeaderOp::Remove(k) => {
                if let Ok(name) = HeaderName::from_bytes(k.as_bytes()) {
                    map.remove(name);
                }
            }
        }
    }
}

/// Map a transport failure to a short, Node-style error code.
pub fn classify(e: &reqwest::Error) -> &'static str {
    use std::error::Error as _;
    use std::io::ErrorKind;
    if e.is_timeout() {
        return "ETIMEDOUT";
    }
    let mut src = e.source();
    while let Some(s) = src {
        if let Some(io) = s.downcast_ref::<std::io::Error>() {
            match io.kind() {
                ErrorKind::ConnectionRefused => return "ECONNREFUSED",
                ErrorKind::ConnectionReset => return "ECONNRESET",
                ErrorKind::ConnectionAborted => return "ECONNABORTED",
                ErrorKind::TimedOut => return "ETIMEDOUT",
                ErrorKind::BrokenPipe => return "EPIPE",
                ErrorKind::AddrNotAvailable => return "EADDRNOTAVAIL",
                _ => {}
            }
            #[cfg(unix)]
            if io.raw_os_error() == Some(libc::EMFILE) {
                return "EMFILE";
            }
        }
        src = s.source();
    }
    let text = format!("{e:?}").to_lowercase();
    if text.contains("dns error") || text.contains("failed to lookup") || text.contains("nodename nor servname") {
        "ENOTFOUND"
    } else if text.contains("certificate") || text.contains("tls") || text.contains("handshake") {
        "ETLS"
    } else if text.contains("connection closed") || text.contains("reset") {
        "ECONNRESET"
    } else if e.is_redirect() {
        "ETOOMANYREDIRECTS"
    } else if e.is_connect() {
        "ECONNECT"
    } else if e.is_body() || e.is_decode() {
        "EBODY"
    } else {
        "ERROR"
    }
}

// ---------------------------------------------------------------------------
// Virtual users
// ---------------------------------------------------------------------------

struct Shared {
    /// One client (and connection pool) per worker thread.
    clients: Vec<reqwest::Client>,
    prepared: Vec<Prepared>,
    base: Arc<BaseVars>,
    overrides: Arc<HashMap<String, String>>,
    data: Option<DataSet>,
    data_counter: AtomicU64,
    recorder: Recorder,
    scripts: Option<ScriptPool>,
    think: Option<(Duration, Duration)>,
    verbose: bool,
    verbose_budget: AtomicU64,
    iterations: AtomicU64,
}

impl Shared {
    fn log(&self, line: impl FnOnce() -> String) {
        if self.verbose && self.verbose_budget.fetch_sub(1, Ordering::Relaxed) > 0 {
            eprintln!("{}", line());
        }
    }
}

struct Vu {
    stop: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

async fn vu_main(sh: Arc<Shared>, index: usize, uid: usize, worker: usize, stop: Arc<AtomicBool>) {
    let shard = uid % sh.recorder.shard_count();
    let pick = |iteration: u64| {
        sh.data.as_ref().and_then(|d| d.pick(index, iteration, sh.data_counter.fetch_add(1, Ordering::Relaxed)))
    };
    let mut row = pick(0);
    let mut vars = VuVars::new(sh.base.clone(), sh.overrides.clone(), row.clone());
    if let Some(p) = &sh.scripts {
        p.init_vu(uid, row.as_ref());
    }
    let per_iteration = sh.data.as_ref().is_some_and(|d| d.per_iteration);
    let mut iteration = 0u64;
    'run: loop {
        if iteration > 0 {
            vars.reset_local();
            if per_iteration {
                row = pick(iteration);
                vars.set_data(row.clone());
            }
            if let Some(p) = &sh.scripts {
                p.set_data(uid, row.as_ref());
            }
        }
        for p in &sh.prepared {
            if stop.load(Ordering::Relaxed) {
                break 'run;
            }
            execute(&sh, p, &mut vars, uid, worker, shard, iteration).await;
            if let Some((lo, hi)) = sh.think {
                let span = hi.saturating_sub(lo).as_micros() as u64;
                let wait = lo + Duration::from_micros(if span == 0 { 0 } else { fastrand::u64(..=span) });
                tokio::time::sleep(wait).await;
            }
        }
        iteration += 1;
        sh.iterations.fetch_add(1, Ordering::Relaxed);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // Yield so an all-cached, instantly failing loop cannot starve the runtime.
        tokio::task::yield_now().await;
    }
    if let Some(p) = &sh.scripts {
        p.drop_vu(uid);
    }
}

async fn execute(
    sh: &Shared,
    p: &Prepared,
    vars: &mut VuVars,
    uid: usize,
    worker: usize,
    shard: usize,
    iteration: u64,
) {
    let mut script_error: Option<String> = None;
    let mut header_ops = Vec::new();

    if let (Some(ids), Some(pool)) = (&p.pre_ids, &sh.scripts) {
        let url = p.url(vars).map(|u| u.to_string()).unwrap_or_else(|_| p.spec.url.render(vars));
        let headers = header_pairs(&p.headers(vars));
        let ctx = ExecContext {
            name: &p.spec.name,
            id: p.spec.index,
            iteration,
            method: p.method.as_str(),
            url: &url,
            headers: &headers,
            body: None,
            response: None,
        };
        let out = pool.exec(uid, ids.clone(), Phase::Pre, &ctx, String::new()).await;
        for op in out.ops {
            vars.apply(op);
        }
        for l in &out.logs {
            sh.log(|| format!("[vu {uid}] {} (pre-request) {l}", p.spec.name));
        }
        if out.skip_request {
            return;
        }
        script_error = out.error;
        header_ops = out.headers;
    }

    let url = match p.url(vars) {
        Ok(u) => u,
        Err(code) => {
            sh.log(|| format!("[vu {uid}] {} → invalid URL `{}`", p.spec.name, p.spec.url.render(vars)));
            record(sh, shard, p, 0, 0, Some(code), script_error.as_deref(), &[], 0, 0);
            return;
        }
    };
    let mut headers = p.headers(vars);
    apply_header_ops(&mut headers, &header_ops);
    let body = p.body(vars);
    let bytes_out = body.as_ref().map_or(0, |(b, _)| b.len() as u64);

    let mut req = reqwest::Request::new(p.method.clone(), url);
    *req.headers_mut() = headers;
    if let Some((b, _)) = body {
        *req.body_mut() = Some(b.into());
    }
    let wants_response = p.test_ids.is_some() && sh.scripts.is_some();
    let req_headers = if wants_response { header_pairs(req.headers()) } else { Vec::new() };
    let req_url = if wants_response { req.url().to_string() } else { String::new() };

    let started = Instant::now();
    let (status, resp_headers, payload, transport_error) = match sh.clients[worker].execute(req).await {
        Ok(resp) => {
            let status = resp.status();
            let header_bytes: u64 = resp.headers().iter().map(|(k, v)| (k.as_str().len() + v.len() + 4) as u64).sum();
            let headers = if wants_response { header_pairs(resp.headers()) } else { Vec::new() };
            match resp.bytes().await {
                Ok(b) => (Some((status, header_bytes)), headers, b, None),
                Err(e) => (Some((status, header_bytes)), headers, Bytes::new(), Some(classify(&e))),
            }
        }
        Err(e) => (None, Vec::new(), Bytes::new(), Some(classify(&e))),
    };
    let latency = started.elapsed();
    let code = status.map_or(0, |(s, _)| s.as_u16());
    let bytes_in = payload.len() as u64 + status.map_or(0, |(_, h)| h);

    let mut tests: Vec<(String, bool)> = Vec::new();
    if let (Some(ids), Some(pool), Some((st, _)), None) = (&p.test_ids, &sh.scripts, status, transport_error) {
        let reason = st.canonical_reason().unwrap_or("");
        let ctx = ExecContext {
            name: &p.spec.name,
            id: p.spec.index,
            iteration,
            method: p.method.as_str(),
            url: &req_url,
            headers: &req_headers,
            body: None,
            response: Some(ExecResponse {
                code,
                status: reason,
                time_ms: latency.as_secs_f64() * 1000.0,
                size: payload.len() as u64,
                headers: resp_headers,
            }),
        };
        let text = String::from_utf8_lossy(&payload).into_owned();
        let out = pool.exec(uid, ids.clone(), Phase::Test, &ctx, text).await;
        for op in out.ops {
            vars.apply(op);
        }
        for l in &out.logs {
            sh.log(|| format!("[vu {uid}] {} (test) {l}", p.spec.name));
        }
        if let Some(f) = &out.first_failure {
            sh.log(|| format!("[vu {uid}] {} → {code}: failed test {f}", p.spec.name));
        }
        if script_error.is_none() {
            script_error = out.error;
        }
        tests = out.tests;
    }
    if let Some(e) = transport_error {
        sh.log(|| format!("[vu {uid}] {} → {e} after {:.1}ms", p.spec.name, latency.as_secs_f64() * 1000.0));
    }
    if let Some(e) = &script_error {
        sh.log(|| format!("[vu {uid}] {} → script error: {e}", p.spec.name));
    }
    record(
        sh,
        shard,
        p,
        latency.as_micros() as u64,
        code,
        transport_error,
        script_error.as_deref(),
        &tests,
        bytes_in,
        bytes_out,
    );
}

#[allow(clippy::too_many_arguments)]
fn record(
    sh: &Shared,
    shard: usize,
    p: &Prepared,
    latency_us: u64,
    status: u16,
    transport_error: Option<&'static str>,
    script_error: Option<&str>,
    tests: &[(String, bool)],
    bytes_in: u64,
    bytes_out: u64,
) {
    sh.recorder.record(
        shard,
        &Sample {
            request: p.spec.index,
            latency_us,
            status,
            transport_error,
            script_error,
            assertions: tests,
            bytes_in,
            bytes_out,
        },
    );
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

pub fn build_client(timeout: Duration, insecure: bool, follow_redirects: bool) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("loadman/", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        .connect_timeout(timeout.min(Duration::from_secs(10)))
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_nodelay(true)
        .redirect(if follow_redirects {
            reqwest::redirect::Policy::limited(10)
        } else {
            reqwest::redirect::Policy::none()
        })
        .danger_accept_invalid_certs(insecure)
        .build()
        .context("cannot build HTTP client")
}

pub async fn run(cfg: RunConfig, mut on_progress: impl FnMut(&Progress)) -> Result<RunOutput> {
    let mut warnings = cfg.plan.warnings.clone();
    let use_scripts = cfg.scripts && !cfg.plan.scripts.is_empty();
    let scripts = if use_scripts {
        let (pool, errors) =
            ScriptPool::start(&cfg.plan.scripts, &cfg.base, cfg.script_threads, cfg.script_timeout, cfg.verbose)?;
        warnings.extend(errors.into_iter().map(|e| format!("{e} (counted as a script error on every run)")));
        Some(pool)
    } else {
        if !cfg.plan.scripts.is_empty() {
            warnings.push(format!("{} script(s) skipped because of --no-scripts", cfg.plan.scripts.len()));
        }
        None
    };
    let prepared =
        cfg.plan.requests.iter().cloned().map(|r| Prepared::new(r, use_scripts)).collect::<Result<Vec<_>>>()?;

    let cpus = std::thread::available_parallelism().map_or(8, |n| n.get());
    let shards = cpus.clamp(4, 64);
    // Thread-per-core: each worker drives its own single-threaded runtime and
    // connection pool, so a VU, its connections and their wake-ups stay on one
    // core instead of bouncing between work-stealing threads.
    let workers = Workers::start(cfg.workers.unwrap_or(cpus))?;
    let clients = (0..workers.len())
        .map(|_| build_client(cfg.timeout, cfg.insecure, cfg.follow_redirects))
        .collect::<Result<Vec<_>>>()?;
    let start = Instant::now();
    let started_at = chrono::Utc::now();
    let shared = Arc::new(Shared {
        clients,
        prepared,
        base: Arc::new(cfg.base),
        overrides: Arc::new(cfg.overrides),
        data: cfg.data,
        data_counter: AtomicU64::new(0),
        recorder: Recorder::new(start, cfg.plan.requests.len(), shards),
        scripts,
        think: cfg.think_time,
        verbose: cfg.verbose,
        verbose_budget: AtomicU64::new(200),
        iterations: AtomicU64::new(0),
    });

    let profile = cfg.profile;
    let duration = profile.duration();
    let mut vus: Vec<Vu> = Vec::new();
    let mut stopping: Vec<JoinHandle<()>> = Vec::new();
    let mut next_uid = 0usize;
    let mut timeline = Timeline::default();
    let mut adjust = |target: u32, vus: &mut Vec<Vu>, stopping: &mut Vec<JoinHandle<()>>| {
        while vus.len() < target as usize {
            let stop = Arc::new(AtomicBool::new(false));
            let worker = next_uid % workers.len();
            let task = workers.spawn(worker, vu_main(shared.clone(), vus.len(), next_uid, worker, stop.clone()));
            next_uid += 1;
            vus.push(Vu { stop, task });
        }
        while vus.len() > target as usize {
            let vu = vus.pop().unwrap();
            vu.stop.store(true, Ordering::Relaxed);
            stopping.push(vu.task);
        }
        stopping.retain(|t| !t.is_finished());
        shared.recorder.active_vus.store(vus.len() as u32, Ordering::Relaxed);
    };

    adjust(profile.target_vus(0), &mut vus, &mut stopping);
    timeline.set_vus(0, vus.len() as u32);
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::from_std(start) + cfg.tick, cfg.tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_sec = 0u64;
    let mut interrupted = false;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = &mut ctrl_c => { interrupted = true; break; }
        }
        let elapsed = start.elapsed();
        if elapsed >= duration {
            break;
        }
        let elapsed_ms = elapsed.as_millis() as u64;
        adjust(profile.target_vus(elapsed_ms), &mut vus, &mut stopping);
        let sec = elapsed.as_secs();
        timeline.set_vus(sec, vus.len() as u32);
        if sec > last_sec {
            last_sec = sec;
            let last = timeline.tick(&shared.recorder, sec);
            on_progress(&Progress {
                elapsed,
                duration,
                vus: vus.len() as u32,
                total_requests: shared.recorder.total.load(Ordering::Relaxed),
                total_errors: shared.recorder.total_errors.load(Ordering::Relaxed),
                last_second: last.as_ref(),
            });
        }
    }

    // Graceful stop: let in-flight requests finish (bounded), then abort.
    for vu in &vus {
        vu.stop.store(true, Ordering::Relaxed);
    }
    shared.recorder.active_vus.store(0, Ordering::Relaxed);
    let elapsed = start.elapsed();
    let mut tasks: Vec<JoinHandle<()>> = vus.into_iter().map(|v| v.task).chain(stopping).collect();
    let grace = cfg.timeout.min(Duration::from_secs(10));
    let wait_all = async {
        for t in tasks.iter_mut() {
            let _ = t.await;
        }
    };
    if tokio::time::timeout(grace, wait_all).await.is_err() {
        for t in &tasks {
            t.abort();
        }
    }
    drop(adjust);
    workers.stop();
    timeline.finish(&shared.recorder, elapsed.as_secs_f64().ceil() as u64);
    // The last second is partial (and holds only stragglers after the stop),
    // so it would show up as a misleading drop; totals still include it.
    let full_secs = elapsed.as_secs();
    timeline.points.retain(|w| w.t < full_secs.max(1));

    Ok(RunOutput {
        aggregate: shared.recorder.aggregate(),
        timeline: timeline.points,
        elapsed,
        started_at,
        interrupted,
        iterations: shared.iterations.load(Ordering::Relaxed),
        warnings,
    })
}

/// Worker threads, each running a current-thread Tokio runtime.
struct Workers {
    handles: Vec<tokio::runtime::Handle>,
    shutdown: Vec<tokio::sync::oneshot::Sender<()>>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Workers {
    fn start(n: usize) -> Result<Workers> {
        let mut w = Workers { handles: Vec::new(), shutdown: Vec::new(), threads: Vec::new() };
        for i in 0..n.max(1) {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            w.handles.push(rt.handle().clone());
            w.shutdown.push(tx);
            w.threads.push(
                std::thread::Builder::new()
                    .name(format!("loadman-io-{i}"))
                    .spawn(move || {
                        rt.block_on(async {
                            let _ = rx.await;
                        });
                        rt.shutdown_background();
                    })
                    .context("cannot start worker thread")?,
            );
        }
        Ok(w)
    }

    fn len(&self) -> usize {
        self.handles.len()
    }

    fn spawn<F: std::future::Future<Output = ()> + Send + 'static>(&self, i: usize, f: F) -> JoinHandle<()> {
        self.handles[i].spawn(f)
    }

    fn stop(self) {
        for tx in self.shutdown {
            let _ = tx.send(());
        }
        for t in self.threads {
            let _ = t.join();
        }
    }
}

/// Raise the open-file limit so thousands of VUs can hold connections
/// (macOS defaults to 256).
pub fn raise_fd_limit() {
    #[cfg(unix)]
    unsafe {
        let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return;
        }
        let mut want = lim.rlim_max.min(1 << 20);
        while want > lim.rlim_cur {
            let new = libc::rlimit { rlim_cur: want, rlim_max: lim.rlim_max };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &new) == 0 {
                return;
            }
            want /= 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_encoding() {
        assert_eq!(form_encode("a b&c=d/é"), "a+b%26c%3Dd%2F%C3%A9");
    }
}
