//! Prometheus metrics for a running test, in the text exposition format:
//! served at `/metrics` (`--prometheus-listen`) and pushed to a Pushgateway
//! (`--prometheus-push`).

use std::convert::Infallible;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::metrics::{Aggregate, RequestAcc};

pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Renders the current metrics.
pub type Source = Arc<dyn Fn() -> String + Send + Sync>;

pub struct Snapshot<'a> {
    pub collection: &'a str,
    /// Folder path and name of each request, by index.
    pub requests: &'a [String],
    pub aggregate: &'a Aggregate,
    pub vus: u32,
    pub iterations: u64,
    pub elapsed: Duration,
}

/// Reads one per-request counter.
type Field = fn(&RequestAcc) -> u64;

fn esc(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn family(o: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} {kind}");
}

pub fn render(s: &Snapshot) -> String {
    let mut o = String::with_capacity(4096);
    family(&mut o, "loadman_info", "gauge", "The collection under test and the loadman version.");
    let _ =
        writeln!(o, "loadman_info{{collection=\"{}\",version=\"{}\"}} 1", esc(s.collection), env!("CARGO_PKG_VERSION"));
    family(&mut o, "loadman_vus", "gauge", "Virtual users running.");
    let _ = writeln!(o, "loadman_vus {}", s.vus);
    family(&mut o, "loadman_elapsed_seconds", "gauge", "Time since the test started.");
    let _ = writeln!(o, "loadman_elapsed_seconds {:.3}", s.elapsed.as_secs_f64());
    family(&mut o, "loadman_iterations_total", "counter", "Completed runs through the selected requests.");
    let _ = writeln!(o, "loadman_iterations_total {}", s.iterations);

    let labels: Vec<String> =
        s.requests.iter().enumerate().map(|(i, name)| format!("request=\"{}\",index=\"{i}\"", esc(name))).collect();
    let counters: [(&str, &str, Field); 8] = [
        ("loadman_requests_total", "Requests sent.", |r| r.count),
        ("loadman_errors_total", "Requests with a transport or script error.", |r| r.error_executions),
        ("loadman_transport_errors_total", "Requests that failed before a full response.", |r| r.transport_errors),
        ("loadman_script_errors_total", "Pre-request or test scripts that threw.", |r| r.script_errors),
        ("loadman_http_errors_total", "Responses with status 400 or higher.", |r| r.http_errors),
        ("loadman_assertion_failures_total", "Requests with at least one failed pm.test.", |r| {
            r.failed_assertion_executions
        }),
        ("loadman_received_bytes_total", "Response bytes received.", |r| r.bytes_in),
        ("loadman_sent_bytes_total", "Request body bytes sent.", |r| r.bytes_out),
    ];
    for (name, help, get) in counters {
        family(&mut o, name, "counter", help);
        for (acc, l) in s.aggregate.requests.iter().zip(&labels) {
            let _ = writeln!(o, "{name}{{{l}}} {}", get(acc));
        }
    }

    let name = "loadman_response_time_seconds";
    family(&mut o, name, "summary", "Time from sending a request to receiving the whole body, since the start.");
    for (acc, l) in s.aggregate.requests.iter().zip(&labels) {
        if acc.count > 0 {
            for q in [0.5, 0.9, 0.95, 0.99] {
                let v = acc.hist.value_at_quantile(q) as f64 / 1e6;
                let _ = writeln!(o, "{name}{{{l},quantile=\"{q}\"}} {v}");
            }
        }
        let _ = writeln!(o, "{name}_sum{{{l}}} {}", acc.sum_us as f64 / 1e6);
        let _ = writeln!(o, "{name}_count{{{l}}} {}", acc.count);
    }

    let mut status: Vec<_> = s.aggregate.status.iter().collect();
    status.sort();
    family(&mut o, "loadman_responses_total", "counter", "Responses by HTTP status code.");
    for (code, n) in status {
        let _ = writeln!(o, "loadman_responses_total{{status=\"{code}\"}} {n}");
    }
    let mut errors: Vec<_> = s.aggregate.errors.iter().collect();
    errors.sort();
    family(&mut o, "loadman_errors_by_code_total", "counter", "Transport and script errors by code.");
    for (code, n) in errors {
        let _ = writeln!(o, "loadman_errors_by_code_total{{code=\"{}\"}} {n}", esc(code));
    }
    o
}

pub async fn bind(addr: SocketAddr) -> Result<TcpListener> {
    TcpListener::bind(addr).await.with_context(|| format!("--prometheus-listen: cannot listen on {addr}"))
}

/// Serves `/metrics` until the task is aborted.
pub fn serve(listener: TcpListener, source: Source) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                // Out of file descriptors, most likely: wait instead of spinning.
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let source = source.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| {
                    let source = source.clone();
                    async move {
                        let resp = match req.uri().path() {
                            "/metrics" => Response::builder()
                                .header("content-type", CONTENT_TYPE)
                                .body(Full::new(Bytes::from(source()))),
                            _ => Response::builder().status(404).body(Full::new(Bytes::from_static(b"see /metrics\n"))),
                        };
                        Ok::<_, Infallible>(resp.expect("valid response"))
                    }
                });
                let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), svc).await;
            });
        }
    })
}

/// `http://host:9091` becomes `http://host:9091/metrics/job/loadman`; a URL
/// that already names a job (and maybe more grouping labels) is kept.
pub fn push_url(base: &str) -> String {
    if base.contains("/metrics/job/") {
        base.to_string()
    } else {
        format!("{}/metrics/job/loadman", base.trim_end_matches('/'))
    }
}

/// Replaces the metrics of the Pushgateway group with `body`.
pub async fn push(client: &reqwest::Client, url: &str, body: String) -> Result<()> {
    let r = client.put(url).header("content-type", CONTENT_TYPE).body(body).send().await?;
    if !r.status().is_success() {
        bail!("{url} answered {}", r.status());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposition() {
        let mut acc = RequestAcc::new();
        for us in [1_000, 2_000, 3_000] {
            acc.hist.record(us).unwrap();
            acc.sum_us += us;
            acc.count += 1;
        }
        acc.http_errors = 1;
        let mut agg = Aggregate { requests: vec![acc, RequestAcc::new()], ..Aggregate::default() };
        agg.status.insert(200, 2);
        agg.status.insert(500, 1);
        let text = render(&Snapshot {
            collection: "Shop \"API\"",
            requests: &["Auth / Login".into(), "idle".into()],
            aggregate: &agg,
            vus: 3,
            iterations: 7,
            elapsed: Duration::from_millis(1500),
        });
        assert!(text.contains("loadman_info{collection=\"Shop \\\"API\\\"\",version="), "{text}");
        assert!(text.contains("loadman_vus 3\n"));
        assert!(text.contains("loadman_requests_total{request=\"Auth / Login\",index=\"0\"} 3\n"));
        assert!(text.contains("loadman_http_errors_total{request=\"Auth / Login\",index=\"0\"} 1\n"));
        assert!(
            text.contains("loadman_response_time_seconds{request=\"Auth / Login\",index=\"0\",quantile=\"0.5\"} 0.002")
        );
        assert!(text.contains("loadman_response_time_seconds_sum{request=\"Auth / Login\",index=\"0\"} 0.006\n"));
        // A request without samples has no quantiles but a zero count.
        assert!(!text.contains("index=\"1\",quantile"));
        assert!(text.contains("loadman_response_time_seconds_count{request=\"idle\",index=\"1\"} 0\n"));
        assert!(text.contains("loadman_responses_total{status=\"500\"} 1\n"));
        // Every sample line belongs to a declared family.
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            let name = line.split(['{', ' ']).next().unwrap();
            let base = name.trim_end_matches("_sum").trim_end_matches("_count");
            assert!(text.contains(&format!("# TYPE {base} ")), "{line}");
        }
    }

    #[test]
    fn push_urls() {
        assert_eq!(push_url("http://pg:9091/"), "http://pg:9091/metrics/job/loadman");
        assert_eq!(push_url("http://pg:9091/metrics/job/ci/run/42"), "http://pg:9091/metrics/job/ci/run/42");
    }
}
