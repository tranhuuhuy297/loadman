//! The per-VU fast path for `http://` and `https://` requests must behave like
//! the pooled client it replaces: same responses, bytes, script data, error
//! codes and protocol.

mod support;

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::time::Duration;

use loadman::collection::{self, Plan, Selection};
use loadman::engine::{self, RunConfig};
use loadman::profile::{LoadProfile, ProfileKind};
use loadman::report::{self, Report};
use loadman::vars::BaseVars;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use support::server::ENCODED_TEXT;

struct Run {
    report: Report,
    fast_requests: u64,
    fast_connections: u64,
}

/// The base URL of a test server, whether its certificate must be accepted
/// although it is self-signed (`--insecure`), and the request timeout.
#[derive(Clone)]
struct Target {
    url: String,
    insecure: bool,
    timeout: Duration,
}

/// Request timeout: generous, so a slow or busy machine cannot turn a normal
/// request into a timeout. Only cases that must time out use `HANG_TIMEOUT`.
const TIMEOUT: Duration = Duration::from_secs(5);
/// For servers that never finish answering; short to keep those runs quick.
const HANG_TIMEOUT: Duration = Duration::from_millis(300);
/// Length of one run.
const RUN: Duration = Duration::from_millis(800);
/// For runs where every request of the plan must run at least once: the VUs
/// go through the plan in order, which takes a while on a slow machine.
const FULL_RUN: Duration = Duration::from_secs(2);

impl From<&String> for Target {
    /// Self-signed certificates are accepted unless a test says otherwise.
    fn from(url: &String) -> Target {
        Target { url: url.clone(), insecure: url.starts_with("https://"), timeout: TIMEOUT }
    }
}

async fn run(
    plan: &Plan,
    target: &Target,
    vus: u32,
    fast_path: bool,
    think_time: Option<Duration>,
    duration: Duration,
) -> Run {
    let cfg = RunConfig {
        plan: plan.clone(),
        profile: LoadProfile::named(ProfileKind::Fixed, vus, duration, None).unwrap(),
        base: BaseVars::default(),
        overrides: HashMap::from([("baseUrl".to_string(), target.url.clone())]),
        data: None,
        timeout: target.timeout,
        script_timeout: Duration::from_secs(1),
        insecure: target.insecure,
        follow_redirects: true,
        think_time: think_time.map(|t| (t, t)),
        tick: Duration::from_millis(100),
        scripts: true,
        script_threads: 1,
        workers: Some(2),
        fast_path,
        verbose: false,
    };
    let profile = cfg.profile.clone();
    let out = engine::run(cfg, |_| {}).await.unwrap();
    let (fast_requests, fast_connections) = (out.fast_path_requests, out.fast_path_connections);
    Run { report: report::build(plan, &profile, out, &[]), fast_requests, fast_connections }
}

/// Runs `plan` on the fast path and on the pooled client, at the same time.
async fn both(plan: &Plan, target: impl Into<Target>, vus: u32, think_time: Option<Duration>) -> (Run, Run) {
    both_for(plan, target, vus, think_time, RUN).await
}

async fn both_for(
    plan: &Plan,
    target: impl Into<Target>,
    vus: u32,
    think_time: Option<Duration>,
    duration: Duration,
) -> (Run, Run) {
    let target = target.into();
    tokio::join!(
        run(plan, &target, vus, true, think_time, duration),
        run(plan, &target, vus, false, think_time, duration)
    )
}

fn request(name: &str, method: &str, path: &str, body: Option<&str>, tests: &[&str]) -> Value {
    let mut item = json!({
        "name": name,
        "request": {"method": method, "url": format!("{{{{baseUrl}}}}{path}"),
                    "header": [{"key": "Content-Type", "value": "text/plain"}]}
    });
    if let Some(raw) = body {
        item["request"]["body"] = json!({"mode": "raw", "raw": raw});
    }
    if !tests.is_empty() {
        item["event"] = json!([{"listen": "test", "script": {"exec": tests}}]);
    }
    item
}

fn plan(items: Vec<Value>) -> Plan {
    let text = json!({"info": {"name": "fast path"}, "item": items}).to_string();
    collection::parse(&text, Path::new("."), &Selection::default()).unwrap()
}

fn text_is(expected: &str) -> String {
    format!("pm.test('body', () => pm.expect(pm.response.text()).to.eql({expected:?}))")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fast_path_matches_the_pooled_client() {
    assert_parity(&support::server::spawn().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn https_fast_path_matches_the_pooled_client() {
    assert_parity(&support::server::spawn_tls(&["http/1.1"]).await).await;
}

/// The same requests on both paths get the same responses, bytes and script
/// data, with every request of the fast run on the fast path.
async fn assert_parity(url: &String) {
    let decoded = [
        text_is(ENCODED_TEXT),
        "pm.test('headers', () => pm.expect(pm.response.headers.has('content-encoding') || pm.response.headers.has('content-length')).to.eql(false))".into(),
    ];
    let decoded: Vec<&str> = decoded.iter().map(String::as_str).collect();
    let host = url.split_once("://").unwrap().1;
    let plan = plan(vec![
        request("hello", "GET", "/hello", None, &[&text_is("ok"), "pm.response.to.have.status(200)"]),
        request("version", "GET", "/version", None, &[&text_is("HTTP/1.1")]),
        request("gzip", "GET", "/encoded/gzip", None, &decoded),
        request("deflate", "GET", "/encoded/deflate", None, &decoded),
        request("br", "GET", "/encoded/br", None, &decoded),
        request("zstd", "GET", "/encoded/zstd", None, &decoded),
        request("post", "POST", "/echo", Some("payload"), &[&text_is("POST payload")]),
        // tower-http's redirect rules: 301/302 turn POST into GET, 303 turns
        // everything into GET, 307/308 keep method and body.
        request("302 post", "POST", "/redirect/302?to=/echo", Some("x"), &[&text_is("GET ")]),
        request("301 put", "PUT", "/redirect/301?to=/echo", Some("kept"), &[&text_is("PUT kept")]),
        request("303 put", "PUT", "/redirect/303?to=/echo", Some("x"), &[&text_is("GET ")]),
        request("307 post", "POST", "/redirect/307?to=/echo", Some("kept"), &[&text_is("POST kept")]),
        request("chain", "GET", "/chain/10", None, &[&text_is("end")]),
        request(
            "referer",
            "GET",
            "/redirect/302?to=/echo-header/referer",
            None,
            &["pm.test('referer', () => pm.expect(pm.response.text()).to.eql(pm.request.url.toString()))"],
        ),
        request("accept-encoding", "GET", "/echo-header/accept-encoding", None, &[&text_is("zstd,gzip,deflate,br")]),
        request("accept", "GET", "/echo-header/accept", None, &[&text_is("*/*")]),
        request(
            "user-agent",
            "GET",
            "/echo-header/user-agent",
            None,
            &[&text_is(concat!("loadman/", env!("CARGO_PKG_VERSION")))],
        ),
        request("host", "GET", "/echo-header/host", None, &[&text_is(host)]),
        request("not found", "GET", "/status/404", None, &["pm.response.to.have.status(404)"]),
        request("head", "HEAD", "/hello", None, &[&text_is(""), "pm.response.to.have.status(200)"]),
    ]);
    let (fast, pooled) = both_for(&plan, url, 3, None, FULL_RUN).await;

    assert!(fast.fast_requests > 0 && pooled.fast_requests == 0);
    assert_same_responses(&fast, &pooled);
}

fn assert_same_responses(fast: &Run, pooled: &Run) {
    for r in [&fast.report, &pooled.report] {
        assert_eq!((r.totals.transport_errors, r.totals.script_errors), (0, 0), "{:?} {:?}", r.errors, r.script_errors);
        assert!(r.assertions.iter().all(|a| a.passed > 0 && a.failed == 0), "{:#?}", r.assertions);
    }
    // Same bytes per response for every request, so the same headers reached
    // the scripts and the same body was decoded.
    for (f, p) in fast.report.requests.iter().zip(&pooled.report.requests) {
        assert!(f.totals.requests > 0 && p.totals.requests > 0, "{}", f.name);
        assert_eq!(f.totals.bytes_in / f.totals.requests, p.totals.bytes_in / p.totals.requests, "{}", f.name);
        assert_eq!(f.totals.bytes_out / f.totals.requests, p.totals.bytes_out / p.totals.requests, "{}", f.name);
        assert_eq!(f.totals.http_errors > 0, p.totals.http_errors > 0, "{}", f.name);
    }
}

/// A server that picks HTTP/2 keeps getting HTTP/2: the fast path hands its
/// requests to the pooled client.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http2_servers_keep_http2() {
    let url = support::server::spawn_tls(&["h2", "http/1.1"]).await;
    let plan = plan(vec![
        request("version", "GET", "/version", None, &[&text_is("HTTP/2.0")]),
        request("post", "POST", "/echo", Some("payload"), &[&text_is("POST payload")]),
        request("gzip", "GET", "/encoded/gzip", None, &[&text_is(ENCODED_TEXT)]),
    ]);
    let (fast, pooled) = both_for(&plan, &url, 3, None, FULL_RUN).await;
    assert_eq!(fast.fast_requests, 0);
    // One TLS handshake per VU at most, before the first one finds out.
    assert!(fast.fast_connections <= 3, "{}", fast.fast_connections);
    assert_same_responses(&fast, &pooled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_vu_keeps_its_connection() {
    for url in [support::server::spawn().await, support::server::spawn_tls(&["http/1.1"]).await] {
        let plan = plan(vec![request("hello", "GET", "/hello", None, &[&text_is("ok")])]);
        let out = run(&plan, &(&url).into(), 4, true, None, RUN).await;
        assert_eq!(out.report.totals.transport_errors, 0, "{url}: {:?}", out.report.errors);
        assert_eq!(out.fast_requests, out.report.totals.requests, "{url}");
        assert_eq!(out.fast_connections, 4, "{url}");
    }
}

/// A raw HTTP/1.1 server that misbehaves on purpose, chosen by the path;
/// over TLS (HTTP/1.1 only) with `tls`.
async fn spawn_raw(tls: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = tls.then(|| support::server::tls_acceptor(&["http/1.1"]));
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { continue };
            match acceptor.clone() {
                None => tokio::spawn(serve_raw(stream)),
                Some(tls) => tokio::spawn(async move {
                    if let Ok(stream) = tls.accept(stream).await {
                        serve_raw(stream).await;
                    }
                }),
            };
        }
    });
    format!("{}://{addr}", if tls { "https" } else { "http" })
}

/// A server that reads the TLS ClientHello and closes the connection without
/// answering. (Reading it all first means the client always sees the close,
/// never a reset.)
async fn spawn_closing() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { continue };
            tokio::spawn(async move {
                let mut header = [0u8; 5];
                if stream.read_exact(&mut header).await.is_ok() {
                    let mut record = vec![0u8; u16::from_be_bytes([header[3], header[4]]) as usize];
                    let _ = stream.read_exact(&mut record).await;
                }
            });
        }
    });
    format!("https://{addr}")
}

trait RawStream: AsyncRead + AsyncWrite + Unpin + Send {
    fn tcp(&self) -> &TcpStream;
}

impl RawStream for TcpStream {
    fn tcp(&self) -> &TcpStream {
        self
    }
}

impl RawStream for tokio_rustls::server::TlsStream<TcpStream> {
    fn tcp(&self) -> &TcpStream {
        self.get_ref().0
    }
}

async fn serve_raw(mut stream: impl RawStream) {
    let mut buf = Vec::new();
    loop {
        // One request head (the requests have no body).
        let mut chunk = [0u8; 4096];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&buf[..end]).to_string();
        buf.drain(..end);
        let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
        let ok = |body: &str| format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}", body.len());
        // The reply, and whether the connection stays usable afterwards.
        let (reply, keep_alive) = match path.as_str() {
            "/close" => return,
            "/reset" => {
                let _ = stream.tcp().set_zero_linger();
                return;
            }
            "/partial-head" => ("HTTP/1.1 200 OK\r\ncontent-le".to_string(), false),
            "/short-body" => ("HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nhello".to_string(), false),
            "/short-chunked" => {
                ("HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n".to_string(), false)
            }
            "/garbage" => ("NOT HTTP AT ALL\r\n\r\n".to_string(), false),
            "/bad-gzip" => {
                ("HTTP/1.1 200 OK\r\ncontent-encoding: gzip\r\ncontent-length: 5\r\n\r\nhello".to_string(), true)
            }
            "/hang" => ("".to_string(), false),
            "/hang-body" => ("HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\nhel".to_string(), false),
            "/bad-scheme" => {
                ("HTTP/1.1 302 Found\r\nlocation: ftp://example.com/\r\ncontent-length: 0\r\n\r\n".to_string(), true)
            }
            // Answers, then closes the idle connection without saying so.
            "/one-shot" => (ok("one"), false),
            _ => (ok("ok"), true),
        };
        if stream.write_all(reply.as_bytes()).await.is_err() || stream.flush().await.is_err() {
            return;
        }
        if path == "/one-shot" {
            // Closed right behind the response, so the close always reaches
            // the client before its next request, however slow the machine.
            let _ = stream.shutdown().await;
            return;
        }
        if path.starts_with("/hang") {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        if !keep_alive {
            tokio::time::sleep(Duration::from_millis(5)).await;
            return;
        }
    }
}

fn codes(r: &Report) -> (BTreeSet<String>, BTreeSet<u16>) {
    (r.errors.iter().map(|(c, _)| c.clone()).collect(), r.status_codes.iter().map(|(s, _)| *s).collect())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn errors_are_classified_like_the_pooled_client() {
    let raw = spawn_raw(false).await;
    let hyper = support::server::spawn().await;
    let cases = [
        (&raw, "/close", "ERROR"),
        (&raw, "/reset", "ECONNRESET"),
        (&raw, "/partial-head", "ERROR"),
        (&raw, "/short-body", "EBODY"),
        (&raw, "/short-chunked", "EBODY"),
        (&raw, "/garbage", "ERROR"),
        (&raw, "/bad-gzip", "EBODY"),
        (&raw, "/bad-scheme", "ERROR"),
        (&hyper, "/chain/11", "ETOOMANYREDIRECTS"),
    ];
    let mut cases: Vec<(Target, &str, &str)> =
        cases.into_iter().map(|(url, path, code)| (url.into(), path, code)).collect();
    cases.push((hanging(&raw), "/hang", "ETIMEDOUT"));
    cases.push((hanging(&raw), "/hang-body", "ETIMEDOUT"));
    cases.push((refused("http"), "/hello", "ECONNREFUSED"));
    assert_same_errors(cases).await;
}

/// A closed port. Windows takes about two seconds to report a refused
/// loopback connection (it retries the SYN first), well within `TIMEOUT`;
/// elsewhere the refusal is immediate.
fn refused(scheme: &str) -> Target {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    Target { url: format!("{scheme}://127.0.0.1:{port}"), insecure: scheme == "https", timeout: TIMEOUT }
}

/// `url` with the short timeout, for servers that never finish answering.
fn hanging(url: &String) -> Target {
    Target { timeout: HANG_TIMEOUT, ..url.into() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn https_errors_are_classified_like_the_pooled_client() {
    let raw = spawn_raw(true).await;
    let tls = support::server::spawn_tls(&["http/1.1"]).await;
    // Plain HTTP and closed connections where a TLS handshake is expected.
    let plain = support::server::spawn().await.replace("http://", "https://");
    let closing = spawn_closing().await;
    let cases = [
        (&raw, "/close", "ETLS"),
        (&raw, "/reset", "ECONNRESET"),
        (&raw, "/partial-head", "ETLS"),
        (&raw, "/short-body", "ETLS"),
        (&raw, "/short-chunked", "ETLS"),
        (&raw, "/garbage", "ERROR"),
        (&raw, "/bad-gzip", "EBODY"),
        (&raw, "/bad-scheme", "ERROR"),
        (&tls, "/chain/11", "ETOOMANYREDIRECTS"),
        (&plain, "/hello", "ECONNECT"),
        (&closing, "/hello", "ETLS"),
    ];
    let mut cases: Vec<(Target, &str, &str)> =
        cases.iter().map(|&(url, path, code)| (url.into(), path, code)).collect();
    // A self-signed certificate without `--insecure`.
    cases.push((Target { url: tls.clone(), insecure: false, timeout: TIMEOUT }, "/hello", "ETLS"));
    cases.push((hanging(&raw), "/hang", "ETIMEDOUT"));
    cases.push((hanging(&raw), "/hang-body", "ETIMEDOUT"));
    cases.push((refused("https"), "/hello", "ECONNREFUSED"));
    assert_same_errors(cases).await;
}

/// Each case fails with `expected` on the fast path, and with the same error
/// codes and statuses on both paths.
async fn assert_same_errors(cases: impl IntoIterator<Item = (Target, &'static str, &'static str)>) {
    let cases: Vec<_> = cases.into_iter().collect();
    // A few cases at a time: starting dozens of runs at once can keep a small
    // CI machine busy long enough for a request to exceed the 300 ms timeout
    // on either path.
    let mut results = Vec::new();
    for batch in cases.chunks(4) {
        let runs = batch.iter().map(|(target, path, _)| {
            let (plan, target) = (plan(vec![request(path, "GET", path, None, &[])]), target.clone());
            // Paced: a tight loop of failing connections would exhaust ephemeral ports.
            async move { both(&plan, target, 1, Some(Duration::from_millis(50))).await }
        });
        results.extend(join_all(runs).await);
    }
    for ((target, path, expected), (fast, pooled)) in cases.iter().zip(results) {
        let case = format!("{}{path} (insecure: {})", target.url, target.insecure);
        assert!(fast.fast_requests > 0, "{case}: not sent on the fast path");
        assert_eq!(codes(&fast.report), codes(&pooled.report), "{case}");
        assert!(codes(&fast.report).0.contains(*expected), "{case}: {:?}", fast.report.errors);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_connections_are_replaced() {
    let (raw, hyper) = (spawn_raw(false).await, support::server::spawn().await);
    let (raw_tls, hyper_tls) = (spawn_raw(true).await, support::server::spawn_tls(&["http/1.1"]).await);
    // The raw server closes the connection right after answering, without
    // saying so; after the pause the next request must go out on a new
    // connection instead of failing.
    let silent = plan(vec![request("one-shot", "GET", "/one-shot", None, &[&text_is("one")])]);
    // `Connection: close` responses.
    let announced = plan(vec![request("close", "GET", "/close", None, &[&text_is("bye")])]);
    let think = Some(Duration::from_millis(50));
    // Long enough for several replacements even on a slow machine.
    let runs = tokio::join!(
        both_for(&silent, &raw, 1, think, FULL_RUN),
        both_for(&announced, &hyper, 1, think, FULL_RUN),
        both_for(&silent, &raw_tls, 1, think, FULL_RUN),
        both_for(&announced, &hyper_tls, 1, think, FULL_RUN),
    );
    for (fast, pooled) in [runs.0, runs.1, runs.2, runs.3] {
        for r in [&fast, &pooled] {
            assert!(r.report.totals.requests >= 3, "{}", r.report.totals.requests);
            assert_eq!(r.report.totals.transport_errors, 0, "{:?}", r.report.errors);
            assert!(r.report.assertions[0].failed == 0);
        }
        assert_eq!(fast.fast_requests, fast.report.totals.requests);
        assert_eq!(fast.fast_connections, fast.fast_requests);
    }
}

/// Runs futures in parallel, without pulling in the futures crate.
async fn join_all<F>(futs: impl IntoIterator<Item = F>) -> Vec<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handles: Vec<_> = futs.into_iter().map(tokio::spawn).collect();
    let mut out = Vec::new();
    for h in handles {
        out.push(h.await.unwrap());
    }
    out
}
