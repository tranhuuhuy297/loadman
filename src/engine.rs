//! The load generator: schedules virtual users along a load profile and runs
//! the collection's requests in a closed loop.
//!
//! Model (see docs/COMPATIBILITY.md): every tick the target VU count is read
//! from the profile; new VUs get the next index,
//! surplus VUs are stopped newest first. Each VU runs the selected requests
//! in order, then starts over, until it is stopped. A stopped VU finishes the
//! request it is executing, which is still recorded.

use std::collections::HashMap;
use std::error::Error as StdError;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine as _;
use bytes::Bytes;
use hyper::Uri;
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, Entry, HeaderMap,
    HeaderName, HeaderValue, LOCATION, PROXY_AUTHORIZATION, REFERER, SET_COOKIE, TRANSFER_ENCODING, USER_AGENT,
    WWW_AUTHENTICATE,
};
use reqwest::{Method, StatusCode, Url};
use tokio::task::JoinHandle;

use crate::collection::{AuthSpec, BodySpec, FormPart, Plan, RequestSpec};
use crate::conn::{self, Conns, Failure, FastPath, Head};
use crate::data::{DataSet, Distribution};
use crate::metrics::{Aggregate, Recorder, Sample, Timeline, WindowSummary};
use crate::profile::LoadProfile;
use crate::script::{CookieOp, ExecContext, ExecResponse, HeaderOp, HttpConfig, Phase, ScriptPool};
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
    /// Send `http://` and `https://` requests over per-VU HTTP/1.1 connections
    /// driven by the VU task itself; `false` sends everything through the
    /// pooled client.
    pub fast_path: bool,
    /// Keep a cookie jar per virtual user.
    pub cookies: bool,
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
    /// Requests sent, and connections opened, on the per-VU fast path.
    pub fast_path_requests: u64,
    pub fast_path_connections: u64,
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
    let stage = if e.is_redirect() {
        Stage::Redirect
    } else if e.is_connect() {
        Stage::Connect
    } else if e.is_body() || e.is_decode() {
        Stage::Body
    } else {
        Stage::Other
    };
    classify_parts(e.is_timeout(), e.source(), stage, || format!("{e:?}"))
}

/// What a failed request was doing, as reqwest's error kinds tell it.
#[derive(Clone, Copy)]
enum Stage {
    Connect,
    Body,
    Redirect,
    Other,
}

/// `classify` for errors from either client: the I/O error in the source
/// chain, else keywords in the error text, else the stage.
fn classify_parts(
    timeout: bool,
    source: Option<&(dyn StdError + 'static)>,
    stage: Stage,
    text: impl FnOnce() -> String,
) -> &'static str {
    use std::io::ErrorKind;
    if timeout {
        return "ETIMEDOUT";
    }
    let mut src = source;
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
    let text = text().to_lowercase();
    if text.contains("dns error") || text.contains("failed to lookup") || text.contains("nodename nor servname") {
        "ENOTFOUND"
    } else if text.contains("certificate") || text.contains("tls") || text.contains("handshake") {
        "ETLS"
    } else if text.contains("connection closed") || text.contains("reset") {
        "ECONNRESET"
    } else {
        match stage {
            Stage::Redirect => "ETOOMANYREDIRECTS",
            Stage::Connect => "ECONNECT",
            Stage::Body => "EBODY",
            Stage::Other => "ERROR",
        }
    }
}

/// `classify` for a fast-path error, with the same text a reqwest error would
/// carry (its URL and source).
fn classify_fast(err: Option<&(dyn StdError + 'static)>, stage: Stage, url: &Url) -> &'static str {
    classify_parts(false, err, stage, || format!("{:?} {err:?}", url.as_str()))
}

// ---------------------------------------------------------------------------
// Virtual users
// ---------------------------------------------------------------------------

struct Shared {
    /// One client (and connection pool) per worker thread.
    clients: Vec<reqwest::Client>,
    /// Per worker, built on first use: follows a redirect received on the
    /// fast path, with one redirect less to go.
    redirect_clients: Vec<OnceLock<reqwest::Client>>,
    /// Per worker, built on first use: does not follow redirects, so
    /// `send_pooled_jar` can store the cookies of every hop.
    manual_clients: Vec<OnceLock<reqwest::Client>>,
    /// setNextRequest targets: request name, folder path and item id.
    request_lookup: HashMap<String, usize>,
    fast: Option<FastPath>,
    /// Headers the pooled client adds to every request that lacks them.
    default_headers: HeaderMap,
    timeout: Duration,
    insecure: bool,
    follow_redirects: bool,
    cookies: bool,
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
    // Random rows are re-drawn every time the VU starts the requests again.
    let per_iteration = sh.data.as_ref().is_some_and(|d| d.per_iteration || d.distribution == Distribution::Random);
    let mut conns = sh.fast.as_ref().map(Conns::new);
    let mut jar = sh.cookies.then(cookie_store::CookieStore::default);
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
        let mut i = 0;
        while let Some(p) = sh.prepared.get(i) {
            if stop.load(Ordering::Relaxed) {
                break 'run;
            }
            let next = execute(&sh, p, &mut vars, &mut conns, &mut jar, uid, worker, shard, iteration).await;
            if let Some((lo, hi)) = sh.think {
                let span = hi.saturating_sub(lo).as_micros() as u64;
                let wait = lo + Duration::from_micros(if span == 0 { 0 } else { fastrand::u64(..=span) });
                tokio::time::sleep(wait).await;
            }
            i = match next {
                None => i + 1,
                // setNextRequest(null) ends the iteration.
                Some(None) => sh.prepared.len(),
                Some(Some(to)) => match sh.request_lookup.get(&to) {
                    Some(&j) => j,
                    None => {
                        sh.log(|| format!("[vu {uid}] setNextRequest: no request named `{to}`; ending the iteration"));
                        sh.prepared.len()
                    }
                },
            };
        }
        iteration += 1;
        sh.iterations.fetch_add(1, Ordering::Relaxed);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // Yield if this VU has run for a while without waiting (an instantly
        // failing loop), so it cannot starve the others. An unconditional
        // `yield_now` here would cost an extra event-loop turn per iteration.
        tokio::task::coop::consume_budget().await;
    }
    if let Some(p) = &sh.scripts {
        p.drop_vu(uid);
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    sh: &Shared,
    p: &Prepared,
    vars: &mut VuVars,
    conns: &mut Option<Conns>,
    jar: &mut Option<cookie_store::CookieStore>,
    uid: usize,
    worker: usize,
    shard: usize,
    iteration: u64,
) -> Option<Option<String>> {
    let mut script_error: Option<String> = None;
    // setNextRequest from either script; the last call wins.
    let mut next: Option<Option<String>> = None;
    let mut header_ops = Vec::new();
    // pm.test results from both scripts, as Postman reports them.
    let mut tests: Vec<(String, bool)> = Vec::new();

    if let (Some(ids), Some(pool)) = (&p.pre_ids, &sh.scripts) {
        let parsed = p.url(vars);
        let cookies = parsed.as_ref().map(|u| jar_cookies(jar, u)).unwrap_or_default();
        let url = parsed.map(|u| u.to_string()).unwrap_or_else(|_| p.spec.url.render(vars));
        let headers = header_pairs(&p.headers(vars));
        let ctx = ExecContext {
            name: &p.spec.name,
            id: p.spec.index,
            item_id: p.spec.item_id.as_deref(),
            iteration,
            method: p.method.as_str(),
            url: &url,
            headers: &headers,
            body: None,
            response: None,
            cookies: &cookies,
        };
        let out = pool.exec(uid, ids.clone(), Phase::Pre, &ctx, String::new()).await;
        for op in out.ops {
            vars.apply(op);
        }
        apply_cookie_ops(jar, &out.cookies);
        for l in &out.logs {
            sh.log(|| format!("[vu {uid}] {} (pre-request) {l}", p.spec.name));
        }
        if out.next_request.is_some() {
            next = out.next_request.clone();
        }
        if out.skip_request {
            return next;
        }
        script_error = out.error;
        header_ops = out.headers;
        if let Some(f) = &out.first_failure {
            sh.log(|| format!("[vu {uid}] {} (pre-request): failed test {f}", p.spec.name));
        }
        tests = out.tests;
    }

    let url = match p.url(vars) {
        Ok(u) => u,
        Err(code) => {
            sh.log(|| format!("[vu {uid}] {} → invalid URL `{}`", p.spec.name, p.spec.url.render(vars)));
            record(sh, shard, p, 0, 0, Some(code), script_error.as_deref(), &[], 0, 0);
            return next;
        }
    };
    let mut headers = p.headers(vars);
    apply_header_ops(&mut headers, &header_ops);
    // The jar's cookies, unless the request sets its own Cookie header.
    let mut jar_cookie = false;
    if let Some(j) = jar.as_ref()
        && !headers.contains_key(COOKIE)
        && let Some(v) = cookie_header(j, &url)
    {
        headers.insert(COOKIE, v);
        jar_cookie = true;
    }
    let jar_url = jar.is_some().then(|| url.clone());
    let body = p.body(vars).map(|(b, _)| b);
    let bytes_out = body.as_ref().map_or(0, |b| b.len() as u64);

    let wants_response = p.test_ids.is_some() && sh.scripts.is_some();
    let req_headers = if wants_response { header_pairs(&headers) } else { Vec::new() };
    let req_url = if wants_response { url.to_string() } else { String::new() };

    let started = Instant::now();
    let outcome = match fast_route(sh, conns, &p.method, &url) {
        Some((conns, origin, uri)) => {
            let req = FastRequest { method: p.method.clone(), url, uri, headers, body, jar_cookie };
            send_fast(sh, worker, conns, origin, req, started, wants_response, jar.as_mut()).await
        }
        None => {
            let mut req = reqwest::Request::new(p.method.clone(), url);
            *req.headers_mut() = headers;
            *req.body_mut() = body.map(Into::into);
            match jar.as_mut() {
                Some(jar) => {
                    let deadline =
                        tokio::time::Instant::from_std(started).checked_add(sh.timeout).unwrap_or_else(far_future);
                    send_pooled_jar(sh, worker, req, wants_response, jar, jar_cookie, deadline, MAX_REDIRECTS).await
                }
                None => send_pooled(&sh.clients[worker], req, wants_response).await,
            }
        }
    };
    let Outcome { status, headers: resp_headers, payload, error: transport_error, set_cookies, url: final_url } =
        outcome;
    if let (Some(j), Some(sent_to)) = (jar.as_mut(), jar_url.as_ref()) {
        let from = final_url.as_ref().unwrap_or(sent_to);
        for c in &set_cookies {
            let _ = j.parse(c, from);
        }
    }
    let latency = started.elapsed();
    let code = status.map_or(0, |(s, _)| s.as_u16());
    let bytes_in = payload.len() as u64 + status.map_or(0, |(_, h)| h);

    if let (Some(ids), Some(pool), Some((st, _)), None) = (&p.test_ids, &sh.scripts, status, transport_error) {
        let reason = st.canonical_reason().unwrap_or("");
        let cookies = jar_url.as_ref().map(|u| jar_cookies(jar, u)).unwrap_or_default();
        let ctx = ExecContext {
            name: &p.spec.name,
            id: p.spec.index,
            item_id: p.spec.item_id.as_deref(),
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
            cookies: &cookies,
        };
        let text = String::from_utf8_lossy(&payload).into_owned();
        let out = pool.exec(uid, ids.clone(), Phase::Test, &ctx, text).await;
        for op in out.ops {
            vars.apply(op);
        }
        apply_cookie_ops(jar, &out.cookies);
        if out.next_request.is_some() {
            next = out.next_request.clone();
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
        tests.extend(out.tests);
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
    next
}

/// What came back for one request.
struct Outcome {
    /// Status and size of the response headers, once a response arrived.
    status: Option<(StatusCode, u64)>,
    /// Response headers, collected only for test scripts.
    headers: Vec<(String, String)>,
    payload: Bytes,
    error: Option<&'static str>,
    /// `Set-Cookie` values of the final response.
    set_cookies: Vec<String>,
    /// The final URL, when redirects were followed and cookies were set.
    url: Option<Url>,
}

impl Outcome {
    fn failed(code: &'static str) -> Outcome {
        Outcome {
            status: None,
            headers: Vec::new(),
            payload: Bytes::new(),
            error: Some(code),
            set_cookies: Vec::new(),
            url: None,
        }
    }

    fn head(status: StatusCode, headers: &HeaderMap, wants_headers: bool) -> Outcome {
        let size = headers.iter().map(|(k, v)| (k.as_str().len() + v.len() + 4) as u64).sum();
        let set_cookies =
            headers.get_all(SET_COOKIE).iter().filter_map(|v| v.to_str().ok().map(String::from)).collect();
        let headers = if wants_headers { header_pairs(headers) } else { Vec::new() };
        Outcome { status: Some((status, size)), headers, payload: Bytes::new(), error: None, set_cookies, url: None }
    }
}

/// The `Cookie` header the jar has for `url`, if any.
fn cookie_header(jar: &cookie_store::CookieStore, url: &Url) -> Option<HeaderValue> {
    let pairs: Vec<String> = jar.get_request_values(url).map(|(n, v)| format!("{n}={v}")).collect();
    (!pairs.is_empty()).then(|| HeaderValue::from_str(&pairs.join("; ")).ok()).flatten()
}

fn jar_cookies(jar: &Option<cookie_store::CookieStore>, url: &Url) -> Vec<(String, String)> {
    jar.as_ref()
        .map(|j| j.get_request_values(url).map(|(n, v)| (n.to_string(), v.to_string())).collect())
        .unwrap_or_default()
}

/// Cookies scripts set or removed through `pm.cookies.jar()`.
fn apply_cookie_ops(jar: &mut Option<cookie_store::CookieStore>, ops: &[CookieOp]) {
    let Some(jar) = jar else { return };
    for op in ops {
        let Ok(url) = Url::parse(&op.url) else { continue };
        let cookie = match &op.value {
            Some(v) => format!("{}={v}", op.name),
            None => format!("{}=; Max-Age=0", op.name),
        };
        let _ = jar.parse(&cookie, &url);
    }
}

async fn send_pooled(client: &reqwest::Client, req: reqwest::Request, wants_headers: bool) -> Outcome {
    match client.execute(req).await {
        Ok(resp) => {
            let mut out = Outcome::head(resp.status(), resp.headers(), wants_headers);
            if !out.set_cookies.is_empty() {
                out.url = Some(resp.url().clone());
            }
            match resp.bytes().await {
                Ok(b) => out.payload = b,
                Err(e) => out.error = Some(classify(&e)),
            }
            out
        }
        Err(e) => Outcome::failed(classify(&e)),
    }
}

/// Name, path and item id of each request, first match wins.
fn request_lookup(plan: &Plan) -> HashMap<String, usize> {
    let mut map = HashMap::new();
    for r in &plan.requests {
        for key in [Some(&r.name), Some(&r.path), r.item_id.as_ref()].into_iter().flatten() {
            map.entry(key.clone()).or_insert(r.index);
        }
    }
    map
}

fn store_cookies(jar: &mut cookie_store::CookieStore, headers: &HeaderMap, url: &Url) {
    for v in headers.get_all(SET_COOKIE) {
        if let Ok(v) = v.to_str() {
            let _ = jar.parse(v, url);
        }
    }
}

/// Puts the jar's cookies for the request's URL on it, replacing a `Cookie`
/// header that came from the jar but not one the request set itself.
fn refresh_cookie(jar: &cookie_store::CookieStore, req: &mut reqwest::Request, jar_cookie: bool) {
    if jar_cookie {
        req.headers_mut().remove(COOKIE);
    }
    if !req.headers().contains_key(COOKIE)
        && let Some(v) = cookie_header(jar, req.url())
    {
        req.headers_mut().insert(COOKIE, v);
    }
}

/// The pooled client, following redirects itself (with the same rules as the
/// fast path) so the cookies of every hop go into the VU's jar, which
/// reqwest's own redirect handling cannot see.
#[allow(clippy::too_many_arguments)]
async fn send_pooled_jar(
    sh: &Shared,
    worker: usize,
    mut req: reqwest::Request,
    wants_headers: bool,
    jar: &mut cookie_store::CookieStore,
    jar_cookie: bool,
    deadline: tokio::time::Instant,
    mut hops_left: usize,
) -> Outcome {
    let client = sh.manual_clients[worker].get_or_init(|| {
        build_client_with(sh.timeout, sh.insecure, reqwest::redirect::Policy::none())
            .expect("the same settings already built the main clients")
    });
    loop {
        *req.timeout_mut() = Some(deadline.saturating_duration_since(tokio::time::Instant::now()));
        let (method, url) = (req.method().clone(), req.url().clone());
        let resend = sh
            .follow_redirects
            .then(|| (req.headers().clone(), req.body().and_then(|b| b.as_bytes()).map(Bytes::copy_from_slice)));
        let resp = match client.execute(req).await {
            Ok(resp) => resp,
            Err(e) => return Outcome::failed(classify(&e)),
        };
        store_cookies(jar, resp.headers(), &url);
        if let Some((headers, body)) = resend
            && let Ok(uri) = url.as_str().parse::<Uri>()
            && let Some(next) = redirect(resp.status(), resp.headers(), method, &uri, &url, headers, body)
        {
            if hops_left == 0 {
                return Outcome::failed("ETOOMANYREDIRECTS");
            }
            hops_left -= 1;
            let _ = resp.bytes().await;
            req = match next {
                Ok(req) => req,
                Err(code) => return Outcome::failed(code),
            };
            refresh_cookie(jar, &mut req, jar_cookie);
            continue;
        }
        let mut out = Outcome::head(resp.status(), resp.headers(), wants_headers);
        // Already stored above.
        out.set_cookies.clear();
        match resp.bytes().await {
            Ok(b) => out.payload = b,
            Err(e) => out.error = Some(classify(&e)),
        }
        return out;
    }
}

/// The VU's connections and origin slot for `url`, when the fast path can send it.
fn fast_route<'c>(
    sh: &Shared,
    conns: &'c mut Option<Conns>,
    method: &Method,
    url: &Url,
) -> Option<(&'c mut Conns, usize, Uri)> {
    let (fast, conns) = (sh.fast.as_ref()?, conns.as_mut()?);
    if !matches!(url.scheme(), "http" | "https") || method == Method::CONNECT {
        return None;
    }
    let uri: Uri = url.as_str().parse().ok()?;
    let origin = conns.origin(fast, &uri)?;
    Some((conns, origin, uri))
}

struct FastRequest {
    method: Method,
    url: Url,
    uri: Uri,
    headers: HeaderMap,
    body: Option<Bytes>,
    /// The `Cookie` header came from the VU's jar (so a redirect may refresh it).
    jar_cookie: bool,
}

/// Sends a request over the VU's own connection, with the pooled client's
/// semantics: total timeout, default headers, redirects, decompression and
/// error codes.
#[allow(clippy::too_many_arguments)]
async fn send_fast(
    sh: &Shared,
    worker: usize,
    conns: &mut Conns,
    origin: usize,
    req: FastRequest,
    started: Instant,
    wants_headers: bool,
    jar: Option<&mut cookie_store::CookieStore>,
) -> Outcome {
    use tokio::time::timeout_at;
    let FastRequest { method, url, uri, mut headers, body, jar_cookie } = req;
    let deadline = tokio::time::Instant::from_std(started).checked_add(sh.timeout).unwrap_or_else(far_future);
    for (name, value) in &sh.default_headers {
        if let Entry::Vacant(e) = headers.entry(name) {
            e.insert(value.clone());
        }
    }
    // A redirect is followed with the request as it was before `Host` was set.
    let resend = sh.follow_redirects.then(|| (headers.clone(), body.clone()));
    let head = timeout_at(deadline, conns.request(origin, method.clone(), &uri, headers, body, started)).await;
    let Head { mut parts, body: reader } = match head {
        Ok(Ok(head)) => head,
        Ok(Err(Failure::Connect(e))) => return Outcome::failed(classify_fast(Some(e.as_ref()), Stage::Connect, &url)),
        Ok(Err(Failure::Request(e))) => return Outcome::failed(classify_fast(Some(&e), Stage::Other, &url)),
        Ok(Err(Failure::ConnectTimeout)) | Err(_) => return Outcome::failed("ETIMEDOUT"),
        // The server picked HTTP/2, which the pooled client speaks.
        Ok(Err(Failure::Http2(unsent))) => {
            let mut req = reqwest::Request::new(unsent.method, url);
            *req.headers_mut() = unsent.headers;
            *req.body_mut() = unsent.body.map(Into::into);
            *req.timeout_mut() = Some(deadline.saturating_duration_since(tokio::time::Instant::now()));
            return match jar {
                Some(jar) => {
                    send_pooled_jar(sh, worker, req, wants_headers, jar, jar_cookie, deadline, MAX_REDIRECTS).await
                }
                None => send_pooled(&sh.clients[worker], req, wants_headers).await,
            };
        }
    };
    if let Some((headers, body)) = resend
        && let Some(next) = redirect(parts.status, &parts.headers, method, &uri, &url, headers, body)
    {
        // Read the redirect's own body so the connection stays usable.
        let _ = timeout_at(deadline, reader.read(conns)).await;
        let mut req = match next {
            Ok(req) => req,
            Err(code) => return Outcome::failed(code),
        };
        *req.timeout_mut() = Some(deadline.saturating_duration_since(tokio::time::Instant::now()));
        // Cookies the redirect set, and the jar's cookies for where it points;
        // later hops store theirs too.
        if let Some(jar) = jar {
            store_cookies(jar, &parts.headers, &url);
            refresh_cookie(jar, &mut req, jar_cookie);
            return send_pooled_jar(sh, worker, req, wants_headers, jar, jar_cookie, deadline, MAX_REDIRECTS - 1).await;
        }
        let client = sh.redirect_clients[worker].get_or_init(|| {
            build_client_with(sh.timeout, sh.insecure, reqwest::redirect::Policy::limited(MAX_REDIRECTS - 1))
                .expect("the same settings already built the main clients")
        });
        return send_pooled(client, req, wants_headers).await;
    }
    let encoding = conn::take_encoding(&mut parts.headers);
    let mut out = Outcome::head(parts.status, &parts.headers, wants_headers);
    match timeout_at(deadline, reader.read(conns)).await {
        Ok(Ok(raw)) => match conn::decode(encoding, raw).await {
            Ok(payload) => out.payload = payload,
            Err(e) => out.error = Some(classify_fast(Some(&e), Stage::Body, &url)),
        },
        Ok(Err(e)) => out.error = Some(classify_fast(Some(&e), Stage::Body, &url)),
        Err(_) => out.error = Some("ETIMEDOUT"),
    }
    out
}

fn far_future() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_secs(86400 * 365 * 30)
}

/// Redirects followed in a row before giving up (reqwest's default policy).
const MAX_REDIRECTS: usize = 10;

/// What the pooled client does with a response that may be a redirect
/// (tower-http's `FollowRedirect` with reqwest's default policy): `None`
/// returns the response as it is, otherwise the next request or an error.
fn redirect(
    status: StatusCode,
    response: &HeaderMap,
    mut method: Method,
    uri: &Uri,
    url: &Url,
    mut headers: HeaderMap,
    mut body: Option<Bytes>,
) -> Option<Result<reqwest::Request, &'static str>> {
    fn drop_payload_headers(headers: &mut HeaderMap) {
        for name in [CONTENT_TYPE, CONTENT_LENGTH, CONTENT_ENCODING, TRANSFER_ENCODING] {
            headers.remove(name);
        }
    }
    match status {
        StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND => {
            if method == Method::POST {
                method = Method::GET;
                body = None;
                drop_payload_headers(&mut headers);
            }
        }
        StatusCode::SEE_OTHER => {
            if method != Method::HEAD {
                method = Method::GET;
            }
            body = None;
            drop_payload_headers(&mut headers);
        }
        StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT => {}
        _ => return None,
    }
    let previous = Url::parse(&uri.to_string()).ok()?;
    let location = std::str::from_utf8(response.get(LOCATION)?.as_bytes()).ok()?;
    let location: Uri = previous.join(location).ok()?.as_str().parse().ok()?;
    let next = match Url::parse(&location.to_string()) {
        Ok(next) => next,
        Err(e) => return Some(Err(classify_fast(Some(&e), Stage::Other, url))),
    };
    if next.scheme() != "http" && next.scheme() != "https" {
        return Some(Err(classify_fast(None, Stage::Other, &next)));
    }
    let cross_host = next.host_str() != previous.host_str()
        || next.port_or_known_default() != previous.port_or_known_default()
        || next.scheme() != previous.scheme();
    if cross_host {
        for name in [AUTHORIZATION, COOKIE, PROXY_AUTHORIZATION, WWW_AUTHENTICATE] {
            headers.remove(name);
        }
        headers.remove("cookie2");
    }
    if !(next.scheme() == "http" && previous.scheme() == "https") {
        let mut referer = previous;
        let _ = referer.set_username("");
        let _ = referer.set_password(None);
        referer.set_fragment(None);
        if let Ok(v) = referer.as_str().parse() {
            headers.insert(REFERER, v);
        }
    }
    let mut req = reqwest::Request::new(method, next);
    *req.headers_mut() = headers;
    *req.body_mut() = body.map(Into::into);
    Some(Ok(req))
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

const USER_AGENT_VALUE: &str = concat!("loadman/", env!("CARGO_PKG_VERSION"));

fn connect_timeout(timeout: Duration) -> Duration {
    timeout.min(Duration::from_secs(10))
}

pub fn build_client(timeout: Duration, insecure: bool, follow_redirects: bool) -> Result<reqwest::Client> {
    let policy = if follow_redirects {
        reqwest::redirect::Policy::limited(MAX_REDIRECTS)
    } else {
        reqwest::redirect::Policy::none()
    };
    build_client_with(timeout, insecure, policy)
}

fn build_client_with(
    timeout: Duration,
    insecure: bool,
    redirect: reqwest::redirect::Policy,
) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(USER_AGENT_VALUE)
        .timeout(timeout)
        .connect_timeout(connect_timeout(timeout))
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_nodelay(true)
        .redirect(redirect)
        .danger_accept_invalid_certs(insecure);
    // Without a system trust store reqwest refuses to build the client at
    // all, even for plain http:// runs.
    if crate::roots::system_missing() {
        if !insecure {
            crate::roots::warn_once();
        }
        let certs = crate::roots::bundled().iter().filter_map(|c| reqwest::Certificate::from_der(c).ok());
        builder = builder.tls_certs_only(certs);
    }
    builder.build().context("cannot build HTTP client")
}

/// What reqwest adds to requests that lack them: its default headers, then
/// tower-http's `Accept-Encoding` for the decoders it enables.
fn default_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(ACCEPT, HeaderValue::from_static("*/*"));
    h.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    h.insert(ACCEPT_ENCODING, HeaderValue::from_static("zstd,gzip,deflate,br"));
    h
}

pub async fn run(cfg: RunConfig, mut on_progress: impl FnMut(&Progress)) -> Result<RunOutput> {
    let mut warnings = cfg.plan.warnings.clone();
    let use_scripts = cfg.scripts && !cfg.plan.scripts.is_empty();
    let prepared =
        cfg.plan.requests.iter().cloned().map(|r| Prepared::new(r, use_scripts)).collect::<Result<Vec<_>>>()?;

    let cpus = std::thread::available_parallelism().map_or(8, |n| n.get());
    let shards = cpus.clamp(4, 64);
    // Compiling scripts and building TLS configurations take tens of
    // milliseconds of CPU; doing it on a blocking thread keeps the caller's
    // runtime (and anything else it serves, such as a test's target server)
    // responsive while runs start.
    let setup = {
        // Scripts see --env-var overrides as environment values.
        let scripts = use_scripts.then(|| {
            let mut base = cfg.base.clone();
            base.environment.extend(cfg.overrides.iter().map(|(k, v)| (k.clone(), v.clone())));
            (cfg.plan.scripts.clone(), base)
        });
        let (script_threads, script_timeout, verbose) = (cfg.script_threads, cfg.script_timeout, cfg.verbose);
        let (worker_count, timeout, insecure) = (cfg.workers.unwrap_or(cpus), cfg.timeout, cfg.insecure);
        let (follow_redirects, fast_path) = (cfg.follow_redirects, cfg.fast_path);
        tokio::task::spawn_blocking(move || -> Result<_> {
            let scripts = scripts
                .map(|(scripts, base)| {
                    let http = HttpConfig { timeout, insecure, follow_redirects };
                    ScriptPool::start(&scripts, &base, script_threads, script_timeout, verbose, http)
                })
                .transpose()?;
            // Thread-per-core: each worker drives its own single-threaded
            // runtime and connection pool, so a VU, its connections and their
            // wake-ups stay on one core instead of bouncing between
            // work-stealing threads.
            let workers = Workers::start(worker_count)?;
            let clients = (0..workers.len())
                .map(|_| build_client(timeout, insecure, follow_redirects))
                .collect::<Result<Vec<_>>>()?;
            let fast = match fast_path {
                true => Some(FastPath::new(connect_timeout(timeout), insecure).context("cannot set up TLS")?),
                false => None,
            };
            Ok((scripts, workers, clients, fast))
        })
    };
    let (scripts, workers, clients, fast) = setup.await.context("run setup failed")??;
    let scripts = match scripts {
        Some((pool, errors)) => {
            warnings.extend(errors.into_iter().map(|e| format!("{e} (counted as a script error on every run)")));
            Some(pool)
        }
        None => {
            if !cfg.plan.scripts.is_empty() {
                warnings.push(format!("{} script(s) skipped because of --no-scripts", cfg.plan.scripts.len()));
            }
            None
        }
    };
    let start = Instant::now();
    let started_at = chrono::Utc::now();
    let shared = Arc::new(Shared {
        redirect_clients: clients.iter().map(|_| OnceLock::new()).collect(),
        manual_clients: clients.iter().map(|_| OnceLock::new()).collect(),
        request_lookup: request_lookup(&cfg.plan),
        clients,
        fast,
        default_headers: default_headers(),
        timeout: cfg.timeout,
        insecure: cfg.insecure,
        follow_redirects: cfg.follow_redirects,
        cookies: cfg.cookies,
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
    // Read after the workers stopped: aborted VUs add their counts when dropped.
    let (fast_path_requests, fast_path_connections) = shared.fast.as_ref().map_or((0, 0), FastPath::totals);

    Ok(RunOutput {
        aggregate: shared.recorder.aggregate(),
        timeline: timeline.points,
        elapsed,
        started_at,
        interrupted,
        iterations: shared.iterations.load(Ordering::Relaxed),
        warnings,
        fast_path_requests,
        fast_path_connections,
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
