//! Distributed runs: `loadman agent` runs its share of a test for a
//! controller (`loadman run --agent URL ...`), which splits the load, follows
//! the agents' progress and merges their results.
//!
//! The protocol is JSON over HTTP with a bearer token:
//!
//! - `GET /v1/status`: version and whether a test is running.
//! - `POST /v1/run` with a [`Job`]: runs it and streams one JSON [`Event`] per
//!   line, progress every second and the result at the end. Closing the
//!   connection stops the test.
//! - `POST /v1/stop`: ends the running test early; its result still arrives.
//!
//! Agents send their histograms, not percentiles, so the merged report is
//! exact. They never read their own disk: the files a collection uses come
//! with the job.

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc};

use crate::collection::{self, Files, Selection};
use crate::data::{DataSet, Distribution};
use crate::engine::{self, ArrivalRate, Progress, RunConfig, RunOutput};
use crate::metrics::{Aggregate, RequestAcc, WindowAcc, WindowSummary};
use crate::profile::LoadProfile;
use crate::vars::BaseVars;

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Largest job an agent accepts (collection, files and data together).
const MAX_JOB_BYTES: usize = 512 << 20;

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

/// Everything an agent needs to run its part of a test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    /// The controller's loadman version; agents only run their own version.
    pub version: String,
    /// The collection file's text.
    pub collection: String,
    /// Body files the collection refers to, by the path it uses, base64.
    pub files: BTreeMap<String, String>,
    pub folders: Vec<String>,
    pub requests: Vec<String>,
    pub environment: HashMap<String, String>,
    pub environment_name: Option<String>,
    pub globals: HashMap<String, String>,
    pub overrides: HashMap<String, String>,
    pub data: Option<JobData>,
    pub profile: LoadProfile,
    pub arrival: Option<ArrivalRate>,
    pub timeout_ms: u64,
    pub script_timeout_ms: u64,
    pub insecure: bool,
    pub follow_redirects: bool,
    pub think_time_ms: Option<(u64, u64)>,
    pub scripts: bool,
    pub script_threads: Option<usize>,
    pub workers: Option<usize>,
    pub fast_path: bool,
    pub cookies: bool,
    /// This agent's part: (index, count).
    pub shard: (usize, usize),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobData {
    pub text: String,
    pub json: bool,
    pub distribution: Distribution,
    pub per_iteration: bool,
}

impl JobData {
    pub fn read(path: &Path, distribution: Distribution, per_iteration: bool) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("cannot read data file {}", path.display()))?;
        let json = crate::data::is_json(path, &text);
        Ok(JobData { text, json, distribution, per_iteration })
    }
}

/// One line of an agent's `/v1/run` response.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    Progress(WireProgress),
    Result(Box<WireOutput>),
    Error(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WireProgress {
    pub elapsed_ms: u64,
    pub vus: u32,
    pub requests: u64,
    pub errors: u64,
    pub last_second: Option<WindowSummary>,
}

/// A [`RunOutput`] with its histograms encoded.
#[derive(Serialize, Deserialize)]
pub struct WireOutput {
    pub requests: Vec<RequestAcc>,
    pub errors: Vec<(String, u64)>,
    pub status: Vec<(u16, u64)>,
    pub assertions: Vec<(usize, String, u64, u64)>,
    pub script_messages: Vec<(String, u64)>,
    /// (second, VUs, JSON-encoded [`WindowAcc`]).
    pub windows: Vec<(u64, u32, String)>,
    pub elapsed_ms: u64,
    pub started_at: String,
    pub interrupted: bool,
    pub iterations: u64,
    pub warnings: Vec<String>,
    pub fast_path_requests: u64,
    pub fast_path_connections: u64,
    pub dropped_iterations: u64,
    pub peak_vus: u32,
}

impl WireOutput {
    fn from_run(out: RunOutput) -> Self {
        let a = out.aggregate;
        WireOutput {
            requests: a.requests,
            errors: a.errors.into_iter().collect(),
            status: a.status.into_iter().collect(),
            assertions: a.assertions.into_iter().map(|((r, n), (p, f))| (r, n, p, f)).collect(),
            script_messages: a.script_messages.into_iter().collect(),
            windows: out.windows,
            elapsed_ms: out.elapsed.as_millis() as u64,
            started_at: out.started_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            interrupted: out.interrupted,
            iterations: out.iterations,
            warnings: out.warnings,
            fast_path_requests: out.fast_path_requests,
            fast_path_connections: out.fast_path_connections,
            dropped_iterations: out.dropped_iterations,
            peak_vus: out.peak_vus,
        }
    }
}

/// Combine the agents' results into one, as if a single machine had run the
/// whole test: counts and histograms add up, per-second windows are merged
/// by second, and the run lasted as long as the longest part.
pub fn merge(parts: Vec<WireOutput>, arrival: Option<ArrivalRate>) -> Result<RunOutput> {
    let mut agg = Aggregate::default();
    let mut windows: BTreeMap<u64, (u32, WindowAcc)> = BTreeMap::new();
    let mut out = RunOutput {
        aggregate: Aggregate::default(),
        timeline: Vec::new(),
        elapsed: Duration::ZERO,
        started_at: chrono::Utc::now(),
        interrupted: false,
        iterations: 0,
        warnings: Vec::new(),
        fast_path_requests: 0,
        fast_path_connections: 0,
        arrival,
        dropped_iterations: 0,
        peak_vus: 0,
        windows: Vec::new(),
    };
    for p in parts {
        if agg.requests.is_empty() {
            agg.requests = p.requests.iter().map(|_| RequestAcc::new()).collect();
        }
        if p.requests.len() != agg.requests.len() {
            bail!("agents ran different requests ({} and {})", p.requests.len(), agg.requests.len());
        }
        for (acc, r) in agg.requests.iter_mut().zip(&p.requests) {
            acc.merge(r);
        }
        for (k, v) in p.errors {
            *agg.errors.entry(k).or_default() += v;
        }
        for (k, v) in p.status {
            *agg.status.entry(k).or_default() += v;
        }
        for (r, n, pass, fail) in p.assertions {
            let e = agg.assertions.entry((r, n)).or_default();
            e.0 += pass;
            e.1 += fail;
        }
        for (k, v) in p.script_messages {
            *agg.script_messages.entry(k).or_default() += v;
        }
        for (t, vus, enc) in p.windows {
            let w: WindowAcc = serde_json::from_str(&enc).context("bad window from an agent")?;
            let e = windows.entry(t).or_insert_with(|| (0, WindowAcc::new()));
            e.0 += vus;
            e.1.merge(&w);
        }
        out.elapsed = out.elapsed.max(Duration::from_millis(p.elapsed_ms));
        if let Ok(t) = chrono::DateTime::parse_from_rfc3339(&p.started_at) {
            out.started_at = out.started_at.min(t.with_timezone(&chrono::Utc));
        }
        out.interrupted |= p.interrupted;
        out.iterations += p.iterations;
        for w in p.warnings {
            if !out.warnings.contains(&w) {
                out.warnings.push(w);
            }
        }
        out.fast_path_requests += p.fast_path_requests;
        out.fast_path_connections += p.fast_path_connections;
        out.dropped_iterations += p.dropped_iterations;
        out.peak_vus += p.peak_vus;
    }
    out.timeline = windows.iter().map(|(t, (vus, w))| WindowSummary::from_acc(*t, *vus, w)).collect();
    out.aggregate = agg;
    Ok(out)
}

/// Paths of the files a collection's bodies refer to (form-data file fields
/// and binary bodies), as written in the collection.
pub fn referenced_files(collection: &Value) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                if let Some(Value::Array(fields)) = m.get("formdata") {
                    for f in fields.iter().filter(|f| f.get("type").and_then(Value::as_str) == Some("file")) {
                        match f.get("src") {
                            Some(Value::String(s)) => out.push(s.clone()),
                            Some(Value::Array(a)) => out.extend(a.iter().filter_map(|s| s.as_str().map(String::from))),
                            _ => {}
                        }
                    }
                }
                if let Some(src) = m.get("file").and_then(|f| f.get("src")).and_then(Value::as_str) {
                    out.push(src.to_string());
                }
                m.values().for_each(|v| walk(v, out));
            }
            Value::Array(a) => a.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(collection, &mut out);
    out.sort();
    out.dedup();
    out
}

/// Reads the files [`referenced_files`] lists, relative to the collection.
pub fn bundle_files(collection: &Value, base_dir: &Path) -> Result<BTreeMap<String, String>> {
    referenced_files(collection)
        .into_iter()
        .map(|src| {
            let path = base_dir.join(&src);
            let data = std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
            Ok((src, STANDARD.encode(data)))
        })
        .collect()
}

impl Job {
    /// A job with the CLI's defaults.
    pub fn new(collection: String, profile: LoadProfile) -> Self {
        Job {
            version: VERSION.into(),
            collection,
            files: BTreeMap::new(),
            folders: Vec::new(),
            requests: Vec::new(),
            environment: HashMap::new(),
            environment_name: None,
            globals: HashMap::new(),
            overrides: HashMap::new(),
            data: None,
            profile,
            arrival: None,
            timeout_ms: 30_000,
            script_timeout_ms: 5_000,
            insecure: false,
            follow_redirects: true,
            think_time_ms: None,
            scripts: true,
            script_threads: None,
            workers: None,
            fast_path: true,
            cookies: true,
            shard: (0, 1),
        }
    }

    /// The agent's side: the run this job describes.
    fn into_config(self, cancel: Arc<Notify>) -> Result<RunConfig> {
        let mut files = Files::new();
        for (path, b64) in self.files {
            files.insert(path, STANDARD.decode(b64).context("bad file encoding")?.into());
        }
        let selection = Selection { folders: self.folders, requests: self.requests };
        let plan = collection::parse_bundle(&self.collection, files, &selection)?;
        let base = BaseVars {
            collection: plan.variables.clone(),
            environment: self.environment,
            environment_name: self.environment_name,
            globals: self.globals,
        };
        let data = self.data.map(|d| DataSet::parse(&d.text, d.json, d.distribution, d.per_iteration)).transpose()?;
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        let ms = Duration::from_millis;
        Ok(RunConfig {
            base,
            overrides: self.overrides,
            data,
            timeout: ms(self.timeout_ms),
            script_timeout: ms(self.script_timeout_ms),
            insecure: self.insecure,
            follow_redirects: self.follow_redirects,
            think_time: self.think_time_ms.map(|(a, b)| (ms(a), ms(b))),
            scripts: self.scripts,
            script_threads: self.script_threads.unwrap_or((cpus / 2).max(1)),
            workers: self.workers,
            fast_path: self.fast_path,
            cookies: self.cookies,
            arrival: self.arrival,
            shard: self.shard,
            keep_windows: true,
            cancel: Some(cancel),
            ..RunConfig::new(plan, self.profile)
        })
    }
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

struct Agent {
    token: String,
    busy: AtomicBool,
    /// Stops the running test.
    cancel: parking_lot::Mutex<Option<Arc<Notify>>>,
}

type Body = BoxBody<Bytes, Infallible>;

/// A response body fed line by line from a channel.
struct Lines(mpsc::UnboundedReceiver<Bytes>);

impl hyper::body::Body for Lines {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.0.poll_recv(cx).map(|line| line.map(|b| Ok(Frame::data(b))))
    }
}

fn json(status: StatusCode, v: Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(v.to_string() + "\n")).boxed())
        .expect("valid response")
}

fn line(ev: &Event) -> Bytes {
    let mut s = serde_json::to_string(ev).unwrap_or_else(|e| format!(r#"{{"error":"{e}"}}"#));
    s.push('\n');
    Bytes::from(s)
}

/// Compares in constant time, so the token cannot be guessed byte by byte.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Serves `/v1/*` on `listener` until the process ends.
pub async fn serve_agent(listener: TcpListener, token: String) -> Result<()> {
    if token.len() < 8 {
        bail!("the agent token must be at least 8 characters");
    }
    let agent = Arc::new(Agent { token, busy: AtomicBool::new(false), cancel: parking_lot::Mutex::new(None) });
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let agent = agent.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let agent = agent.clone();
                async move { Ok::<_, Infallible>(handle(agent, req).await) }
            });
            let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), svc).await;
        });
    }
}

pub async fn bind_agent(addr: SocketAddr) -> Result<TcpListener> {
    TcpListener::bind(addr).await.with_context(|| format!("cannot listen on {addr}"))
}

async fn handle(agent: Arc<Agent>, req: Request<Incoming>) -> Response<Body> {
    let auth = req.headers().get("authorization").map(|v| v.as_bytes()).unwrap_or_default();
    let expected = format!("Bearer {}", agent.token);
    if !same(auth, expected.as_bytes()) {
        return json(StatusCode::UNAUTHORIZED, serde_json::json!({ "error": "missing or wrong agent token" }));
    }
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/v1/status") => json(
            StatusCode::OK,
            serde_json::json!({
                "version": VERSION,
                "busy": agent.busy.load(Ordering::SeqCst),
                "cpus": std::thread::available_parallelism().map_or(0, |n| n.get()),
            }),
        ),
        (&Method::POST, "/v1/stop") => {
            let running = agent.cancel.lock().clone();
            if let Some(n) = &running {
                n.notify_one();
            }
            json(StatusCode::OK, serde_json::json!({ "stopping": running.is_some() }))
        }
        (&Method::POST, "/v1/run") => start(agent, req).await,
        _ => json(StatusCode::NOT_FOUND, serde_json::json!({ "error": "unknown endpoint" })),
    }
}

async fn start(agent: Arc<Agent>, req: Request<Incoming>) -> Response<Body> {
    let bad = |msg: String| json(StatusCode::BAD_REQUEST, serde_json::json!({ "error": msg }));
    let body = match Limited::new(req.into_body(), MAX_JOB_BYTES).collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => return bad(format!("cannot read the job: {e}")),
    };
    let job: Job = match serde_json::from_slice(&body) {
        Ok(j) => j,
        Err(e) => return bad(format!("not a loadman job: {e}")),
    };
    if job.version != VERSION {
        return bad(format!("this agent runs loadman {VERSION}, the controller {}", job.version));
    }
    if agent.busy.swap(true, Ordering::SeqCst) {
        return json(StatusCode::CONFLICT, serde_json::json!({ "error": "this agent is already running a test" }));
    }
    let cancel = Arc::new(Notify::new());
    let cfg = match job.into_config(cancel.clone()) {
        Ok(c) => c,
        Err(e) => {
            agent.busy.store(false, Ordering::SeqCst);
            return bad(format!("{e:#}"));
        }
    };
    *agent.cancel.lock() = Some(cancel.clone());
    let (tx, rx) = mpsc::unbounded_channel();
    // The engine runs on its own runtime, as it does for `loadman run`.
    std::thread::spawn(move || {
        let outcome =
            tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(anyhow::Error::from).and_then(
                |rt| {
                    let progress_tx = tx.clone();
                    let stop = cancel.clone();
                    let out = rt.block_on(engine::run(cfg, move |p: &Progress| {
                        let ev = Event::Progress(WireProgress {
                            elapsed_ms: p.elapsed.as_millis() as u64,
                            vus: p.vus,
                            requests: p.total_requests,
                            errors: p.total_errors,
                            last_second: p.last_second.cloned(),
                        });
                        // The controller went away: stop instead of running unwatched.
                        if progress_tx.send(line(&ev)).is_err() {
                            stop.notify_one();
                        }
                    }));
                    rt.shutdown_timeout(Duration::from_secs(1));
                    out
                },
            );
        let ev = match outcome {
            Ok(out) => Event::Result(Box::new(WireOutput::from_run(out))),
            Err(e) => Event::Error(format!("{e:#}")),
        };
        let _ = tx.send(line(&ev));
        *agent.cancel.lock() = None;
        agent.busy.store(false, Ordering::SeqCst);
    });
    Response::builder().header("content-type", "application/x-ndjson").body(Lines(rx).boxed()).expect("valid response")
}

// ---------------------------------------------------------------------------
// Controller
// ---------------------------------------------------------------------------

fn agent_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.contains("://") { format!("{base}{path}") } else { format!("http://{base}{path}") }
}

/// Runs `job` on `agents`, each with its share of the load, and merges the
/// results. `on_progress` sees the whole run's numbers once a second.
pub async fn run_on_agents(
    agents: &[String],
    token: &str,
    job: Job,
    mut on_progress: impl FnMut(&Progress),
) -> Result<RunOutput> {
    let n = agents.len();
    if n == 0 {
        bail!("no agents");
    }
    let mut urls: Vec<String> = agents.iter().map(|a| agent_url(a, "")).collect();
    urls.sort();
    if let Some(w) = urls.windows(2).find(|w| w[0] == w[1]) {
        bail!("agent {} is listed twice", w[0]);
    }
    let client = engine::control_client()?;
    // Check every agent before any of them starts sending traffic.
    for a in agents {
        let r = client
            .get(agent_url(a, "/v1/status"))
            .bearer_auth(token)
            .send()
            .await
            .with_context(|| format!("agent {a} does not answer"))?;
        let status = r.status();
        let v = json_body(r).await;
        if !status.is_success() {
            bail!("agent {a}: {}", v["error"].as_str().unwrap_or(status.as_str()));
        }
        if v["version"].as_str() != Some(VERSION) {
            bail!("agent {a} runs loadman {}, this is {VERSION}", v["version"].as_str().unwrap_or("?"));
        }
        if v["busy"].as_bool() == Some(true) {
            bail!("agent {a} is already running a test");
        }
    }
    let duration = job.profile.duration();
    let arrival = job.arrival;
    let mut parts = Vec::with_capacity(n);
    for i in 0..n {
        let mut part = job.clone();
        part.shard = (i, n);
        match job.arrival {
            Some(a) => part.arrival = Some(a.share(i, n)?),
            None => part.profile = job.profile.share(i, n)?,
        }
        parts.push(part);
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<(usize, Result<Event>)>();
    for (i, part) in parts.into_iter().enumerate() {
        let (tx, client, url, token) =
            (tx.clone(), client.clone(), agent_url(&agents[i], "/v1/run"), token.to_string());
        tokio::spawn(async move {
            if let Err(e) = follow(&client, &url, &token, &part, |ev| {
                let _ = tx.send((i, Ok(ev)));
            })
            .await
            {
                let _ = tx.send((i, Err(e)));
            }
        });
    }
    drop(tx);

    let mut progress = vec![WireProgress::default(); n];
    let mut done = vec![false; n];
    let mut results: Vec<Option<WireOutput>> = (0..n).map(|_| None).collect();
    let mut failures = Vec::new();
    let mut last_sec = 0;
    let mut stopped = false;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        let (i, ev) = tokio::select! {
            ev = rx.recv() => match ev { Some(ev) => ev, None => break },
            _ = &mut ctrl_c, if !stopped => {
                stopped = true;
                for a in agents {
                    let _ = client.post(agent_url(a, "/v1/stop")).bearer_auth(token).send().await;
                }
                continue;
            }
        };
        match ev {
            Ok(Event::Progress(p)) => {
                progress[i] = p;
                // Show a second once every agent still running has reported it.
                let running = progress.iter().zip(&done).filter(|(_, d)| !**d);
                let reported = running.map(|(p, _)| p.elapsed_ms / 1000).min().unwrap_or(0);
                if reported > last_sec {
                    last_sec = reported;
                    let elapsed = progress.iter().map(|p| p.elapsed_ms).max().unwrap_or(0);
                    let last = merge_seconds(progress.iter().filter_map(|p| p.last_second.as_ref()));
                    on_progress(&Progress {
                        elapsed: Duration::from_millis(elapsed),
                        duration,
                        vus: progress.iter().map(|p| p.vus).sum(),
                        total_requests: progress.iter().map(|p| p.requests).sum(),
                        total_errors: progress.iter().map(|p| p.errors).sum(),
                        last_second: last.as_ref(),
                    });
                }
            }
            Ok(Event::Result(out)) => {
                results[i] = Some(*out);
                done[i] = true;
            }
            Ok(Event::Error(e)) => {
                failures.push(format!("agent {}: {e}", agents[i]));
                done[i] = true;
            }
            Err(e) => {
                failures.push(format!("agent {}: {e:#}", agents[i]));
                done[i] = true;
            }
        }
    }
    let parts: Vec<WireOutput> = results.into_iter().flatten().collect();
    if parts.is_empty() {
        bail!("{}", if failures.is_empty() { "no agent returned a result".into() } else { failures.join("; ") });
    }
    let complete = parts.len() == n;
    let mut out = merge(parts, arrival)?;
    if !complete {
        out.warnings.extend(failures.iter().map(|f| format!("{f}; the report leaves its part out")));
    }
    out.interrupted |= stopped;
    Ok(out)
}

/// Approximate whole-run numbers for the progress line from the agents'
/// last seconds (the report itself is merged exactly).
fn merge_seconds<'a>(mut seconds: impl Iterator<Item = &'a WindowSummary>) -> Option<WindowSummary> {
    let mut m = seconds.next()?.clone();
    for w in seconds {
        let total = m.requests + w.requests;
        if total > 0 {
            m.avg_ms = (m.avg_ms * m.requests as f64 + w.avg_ms * w.requests as f64) / total as f64;
        }
        m.requests = total;
        m.rps += w.rps;
        m.vus += w.vus;
        m.p90_ms = m.p90_ms.max(w.p90_ms);
        m.p95_ms = m.p95_ms.max(w.p95_ms);
        m.p99_ms = m.p99_ms.max(w.p99_ms);
        m.max_ms = m.max_ms.max(w.max_ms);
    }
    Some(m)
}

async fn json_body(r: reqwest::Response) -> Value {
    let bytes = r.bytes().await.unwrap_or_default();
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// Starts `job` on one agent and hands each event to `on_event`.
async fn follow(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    job: &Job,
    mut on_event: impl FnMut(Event),
) -> Result<()> {
    let mut r = client
        .post(url)
        .bearer_auth(token)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(job)?)
        .send()
        .await
        .context("cannot start the test")?;
    if !r.status().is_success() {
        let status = r.status();
        bail!("{}", json_body(r).await["error"].as_str().unwrap_or(status.as_str()));
    }
    let mut buf: Vec<u8> = Vec::new();
    let mut finished = false;
    while let Some(chunk) = r.chunk().await.map_err(|e| anyhow!("lost the connection: {e}"))? {
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let rest = buf.split_off(pos + 1);
            let ev: Event = serde_json::from_slice(&buf[..pos]).context("bad event from the agent")?;
            buf = rest;
            finished |= matches!(ev, Event::Result(_) | Event::Error(_));
            on_event(ev);
        }
    }
    if !finished {
        return Err(anyhow!("the agent closed the connection before the test ended"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_body_files() {
        let c: Value = serde_json::from_str(
            r#"{"item":[{"request":{"body":{"mode":"formdata","formdata":[
                {"key":"a","type":"file","src":"a.png"},{"key":"b","type":"file","src":["b.txt","a.png"]},
                {"key":"c","type":"text","value":"x"}]}}},
              {"item":[{"request":{"body":{"mode":"file","file":{"src":"bin/blob"}}}}]}]}"#,
        )
        .unwrap();
        assert_eq!(referenced_files(&c), ["a.png", "b.txt", "bin/blob"]);
    }

    #[test]
    fn tokens_compare_whole() {
        assert!(same(b"Bearer secret", b"Bearer secret"));
        assert!(!same(b"Bearer secre", b"Bearer secret"));
        assert!(!same(b"Bearer secreT", b"Bearer secret"));
    }

    #[test]
    fn urls() {
        assert_eq!(agent_url("10.0.0.5:7070", "/v1/run"), "http://10.0.0.5:7070/v1/run");
        assert_eq!(agent_url("https://agent.example/", "/v1/run"), "https://agent.example/v1/run");
    }
}
