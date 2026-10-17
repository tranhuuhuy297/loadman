//! Tiny HTTP target used by the end-to-end tests and the benchmark.
//!
//! GET  /hello            -> 200 "ok" (HEAD too)
//! POST /login            -> 200 {"token":"secret-token"}
//! GET  /users/{id}       -> 200 JSON user, 401 without `Authorization: Bearer secret-token`
//! GET  /status/{code}    -> that status
//! GET  /slow?ms=N        -> 200 after N milliseconds
//! GET  /echo-header/{h}  -> 200 with the value of request header {h} as body
//! ANY  /echo             -> 200 "{method} {body}", echoing the request's content-type
//! GET  /encoded/{enc}    -> 200 text compressed with gzip, deflate, br or zstd
//! ANY  /redirect/{code}?to=URL -> that redirect status with `Location: URL`
//! GET  /chain/{n}        -> 302 to /chain/{n-1}, 200 "end" at 0
//! GET  /close            -> 200 "bye" with `Connection: close`
//! GET  /version          -> 200 with the request's HTTP version, e.g. "HTTP/1.1"
//!
//! `serve_tls` serves the same routes over TLS with a self-signed certificate
//! for `localhost` and `127.0.0.1`, as HTTP/2 or HTTP/1.1 as ALPN decides.

use std::convert::Infallible;
use std::sync::Arc;

use async_compression::tokio::bufread::{BrotliEncoder, GzipEncoder, ZlibEncoder, ZstdEncoder};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::{http1, http2};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// The text `/encoded/{enc}` sends, before compression.
#[allow(dead_code)]
pub const ENCODED_TEXT: &str = "hello from a compressed body";

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
    let method = req.method().to_string();
    Ok(match (method.as_str(), parts.as_slice()) {
        ("GET" | "HEAD", ["hello"]) => reply(200, "text/plain", "ok"),
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
        (_, ["echo"]) => {
            let ct = req.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("none").to_string();
            let body = req.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
            reply(200, &ct, format!("{method} {}", String::from_utf8_lossy(&body)))
        }
        ("GET", ["encoded", enc]) => {
            let mut out = Vec::new();
            let text = ENCODED_TEXT.as_bytes();
            let _ = match *enc {
                "gzip" => GzipEncoder::new(text).read_to_end(&mut out).await,
                "deflate" => ZlibEncoder::new(text).read_to_end(&mut out).await,
                "br" => BrotliEncoder::new(text).read_to_end(&mut out).await,
                _ => ZstdEncoder::new(text).read_to_end(&mut out).await,
            };
            let mut r = reply(200, "text/plain", out);
            r.headers_mut().insert("content-encoding", enc.parse().unwrap());
            r
        }
        (_, ["redirect", code]) => {
            let to = query.strip_prefix("to=").unwrap_or("/hello").to_string();
            let mut r = reply(code.parse().unwrap_or(302), "text/plain", "moved");
            r.headers_mut().insert("location", to.parse().unwrap());
            r
        }
        ("GET", ["chain", n]) => match n.parse::<u32>().unwrap_or(0) {
            0 => reply(200, "text/plain", "end"),
            n => {
                let mut r = reply(302, "text/plain", "moved");
                r.headers_mut().insert("location", format!("/chain/{}", n - 1).parse().unwrap());
                r
            }
        },
        ("GET", ["version"]) => reply(200, "text/plain", format!("{:?}", req.version())),
        ("GET", ["close"]) => {
            let mut r = reply(200, "text/plain", "bye");
            r.headers_mut().insert("connection", "close".parse().unwrap());
            r
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

/// `serve` over TLS.
pub async fn serve_tls(listener: TcpListener, acceptor: TlsAcceptor) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let _ = stream.set_nodelay(true);
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let Ok(stream) = acceptor.accept(stream).await else { return };
            let h2 = stream.get_ref().1.alpn_protocol() == Some(b"h2");
            let io = TokioIo::new(stream);
            let _ = if h2 {
                http2::Builder::new(TokioExecutor::new()).serve_connection(io, service_fn(handle)).await
            } else {
                http1::Builder::new().keep_alive(true).serve_connection(io, service_fn(handle)).await
            };
        });
    }
}

/// TLS with the self-signed test certificate, offering `alpn` protocols.
pub fn tls_acceptor(alpn: &[&str]) -> TlsAcceptor {
    let cert = CertificateDer::from_pem_slice(include_bytes!("tls/cert.pem")).unwrap();
    let key = PrivateKeyDer::from_pem_slice(include_bytes!("tls/key.pem")).unwrap();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    config.alpn_protocols = alpn.iter().map(|p| p.as_bytes().to_vec()).collect();
    TlsAcceptor::from(Arc::new(config))
}

/// Start the server on an ephemeral port and return its base URL.
#[allow(dead_code)]
pub async fn spawn() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(listener));
    format!("http://{addr}")
}

/// Start the server over TLS, offering `alpn`, and return its base URL.
#[allow(dead_code)]
pub async fn spawn_tls(alpn: &[&str]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_tls(listener, tls_acceptor(alpn)));
    format!("https://{addr}")
}
