//! Tiny HTTP target used by the end-to-end tests and the benchmark.
//!
//! GET  /hello            -> 200 "ok"
//! POST /login            -> 200 {"token":"secret-token"}
//! GET  /users/{id}       -> 200 JSON user, 401 without `Authorization: Bearer secret-token`
//! GET  /status/{code}    -> that status
//! GET  /slow?ms=N        -> 200 after N milliseconds
//! GET  /echo-header/{h}  -> 200 with the value of request header {h} as body

use std::convert::Infallible;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

fn reply(status: u16, content_type: &str, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header("content-type", content_type)
        .body(Full::new(body.into()))
        .unwrap()
}

async fn handle(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let authed = req.headers().get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer secret-token");
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    Ok(match (req.method().as_str(), parts.as_slice()) {
        ("GET", ["hello"]) => reply(200, "text/plain", "ok"),
        ("POST", ["login"]) => reply(200, "application/json", r#"{"token":"secret-token"}"#),
        ("GET", ["users", id]) if authed => {
            reply(200, "application/json", format!(r#"{{"id":"{id}","name":"user-{id}","roles":["reader"]}}"#))
        }
        ("GET", ["users", _]) => reply(401, "application/json", r#"{"error":"unauthorized"}"#),
        ("GET", ["status", code]) => reply(code.parse().unwrap_or(500), "text/plain", "status"),
        ("GET", ["slow"]) => {
            let ms: u64 = query.strip_prefix("ms=").and_then(|v| v.parse().ok()).unwrap_or(50);
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            reply(200, "text/plain", "slow")
        }
        ("GET", ["echo-header", h]) => {
            let v = req.headers().get(*h).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            reply(200, "text/plain", v)
        }
        _ => reply(404, "text/plain", "not found"),
    })
}

pub async fn serve(listener: TcpListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let _ = stream.set_nodelay(true);
        tokio::spawn(async move {
            let _ =
                http1::Builder::new().keep_alive(true).serve_connection(TokioIo::new(stream), service_fn(handle)).await;
        });
    }
}

/// Start the server on an ephemeral port and return its base URL.
#[allow(dead_code)]
pub async fn spawn() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(listener));
    format!("http://{addr}")
}
