//! Per-VU HTTP/1.1 connections: the fast path for `http://` and `https://`
//! requests.
//!
//! The pooled client (reqwest on hyper-util) hands every request to a
//! separate connection task through channels, so each request costs several
//! cross-task wake-ups. Here each VU owns one connection per origin and polls
//! it from its own task while a request is in flight: the VU task writes the
//! request and reads the response itself.
//!
//! Everything around the wire format mirrors the pooled client: connections
//! come from hyper-util's `HttpConnector` with reqwest's settings, TLS uses
//! the rustls configuration reqwest builds, requests get the same default
//! headers, `Host` and origin-form target, an idle connection the server
//! closed is replaced and the unsent request retried, and bodies are decoded
//! like tower-http's decompression layer. Redirects are handled by the engine.
//!
//! TLS offers ALPN `h2` and `http/1.1`, like reqwest. When a server picks
//! HTTP/2, the request is handed back unsent and that origin goes through the
//! pooled client from then on, so it keeps getting HTTP/2.

use std::collections::HashSet;
use std::future::{Future, poll_fn};
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1::{self, SendRequest};
use hyper::header::{CONTENT_ENCODING, CONTENT_LENGTH, HOST, HeaderMap, HeaderValue};
use hyper::http::response::Parts;
use hyper::http::uri::Scheme;
use hyper::{Method, Request, Uri};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::proxy::matcher::Matcher;
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use rustls::ClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio_rustls::TlsConnector;
use tower_service::Service;

type Body = Full<Bytes>;
/// A connection's I/O, plain or TLS: hyper's connection future.
type Driver = Pin<Box<dyn Future<Output = hyper::Result<()>> + Send>>;

/// Connections idle for longer are replaced (the pooled client's idle timeout).
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Why a request failed before a response head arrived.
pub enum Failure {
    /// DNS or TCP connect error (hyper-util's `ConnectError`).
    Connect(Box<dyn std::error::Error + Send + Sync>),
    /// Connecting took longer than the connect timeout.
    ConnectTimeout,
    /// Writing the request or reading the response head failed.
    Request(hyper::Error),
    /// The server picked HTTP/2: send the request with the pooled client.
    Http2(Box<Unsent>),
}

/// A request handed back without having been sent.
pub struct Unsent {
    pub method: Method,
    pub headers: HeaderMap,
    pub body: Option<Bytes>,
}

/// Settings shared by all VUs.
pub struct FastPath {
    connector: HttpConnector,
    tls: TlsConnector,
    proxy: Matcher,
    connect_timeout: Duration,
    /// `https://` authorities whose server picked HTTP/2.
    h2: Arc<Mutex<HashSet<String>>>,
    totals: Arc<Totals>,
}

/// Requests sent and connections opened on the fast path, by all VUs.
#[derive(Default)]
struct Totals {
    requests: AtomicU64,
    connections: AtomicU64,
}

impl FastPath {
    pub fn new(connect_timeout: Duration, insecure: bool) -> Result<FastPath, rustls::Error> {
        // Same TCP settings as reqwest's defaults (plus `tcp_nodelay(true)`).
        let mut connector = HttpConnector::new();
        connector.enforce_http(false);
        connector.set_nodelay(true);
        connector.set_connect_timeout(Some(connect_timeout));
        connector.set_keepalive(Some(Duration::from_secs(15)));
        connector.set_keepalive_interval(Some(Duration::from_secs(15)));
        connector.set_keepalive_retries(Some(3));
        Ok(FastPath {
            connector,
            tls: TlsConnector::from(Arc::new(tls_config(insecure)?)),
            proxy: Matcher::from_system(),
            connect_timeout,
            h2: Arc::default(),
            totals: Arc::default(),
        })
    }

    /// Requests sent and connections opened so far, counting VUs that have
    /// ended (or were aborted).
    pub fn totals(&self) -> (u64, u64) {
        (self.totals.requests.load(Ordering::Relaxed), self.totals.connections.load(Ordering::Relaxed))
    }
}

/// The rustls configuration reqwest's client builder makes with the `rustls`
/// feature and loadman's settings: the installed (else aws-lc-rs) crypto
/// provider, every TLS version, the platform verifier or none with
/// `--insecure`, SNI on, no client certificate, ALPN `h2` then `http/1.1`.
fn tls_config(insecure: bool) -> Result<ClientConfig, rustls::Error> {
    let provider = CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    let builder = ClientConfig::builder_with_provider(provider.clone()).with_protocol_versions(rustls::ALL_VERSIONS)?;
    let verifier: Arc<dyn ServerCertVerifier> =
        if insecure { Arc::new(NoVerifier) } else { Arc::new(rustls_platform_verifier::Verifier::new(provider)?) };
    let mut config = builder.dangerous().with_custom_certificate_verifier(verifier).with_no_client_auth();
    config.enable_sni = true;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

/// Accepts any certificate (reqwest's verifier for `danger_accept_invalid_certs`).
#[derive(Debug)]
struct NoVerifier;

impl ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer,
        _intermediates: &[CertificateDer],
        _server_name: &ServerName,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA1,
            SignatureScheme::ECDSA_SHA1_Legacy,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::ED448,
        ]
    }
}

struct Conn {
    sender: SendRequest<Body>,
    /// `None` once the connection has shut down.
    io: Option<Driver>,
    last_used: Instant,
}

struct Origin {
    https: bool,
    authority: String,
    /// Sent by the pooled client: routed through a proxy by the system
    /// settings, or the server picked HTTP/2.
    pooled: bool,
    host: HeaderValue,
    conn: Option<Conn>,
}

/// The connections owned by one VU, one per origin.
pub struct Conns {
    connector: HttpConnector,
    tls: TlsConnector,
    connect_timeout: Duration,
    h2: Arc<Mutex<HashSet<String>>>,
    origins: Vec<Origin>,
    /// Requests sent and connections opened, added to the totals on drop.
    requests: u64,
    connections: u64,
    totals: Arc<Totals>,
}

impl Drop for Conns {
    // On drop rather than when the VU loop ends: a VU still waiting on a
    // request when the run's grace period ends is aborted.
    fn drop(&mut self) {
        self.totals.requests.fetch_add(self.requests, Ordering::Relaxed);
        self.totals.connections.fetch_add(self.connections, Ordering::Relaxed);
    }
}

impl Conns {
    pub fn new(fast: &FastPath) -> Conns {
        Conns {
            connector: fast.connector.clone(),
            tls: fast.tls.clone(),
            connect_timeout: fast.connect_timeout,
            h2: fast.h2.clone(),
            origins: Vec::new(),
            requests: 0,
            connections: 0,
            totals: fast.totals.clone(),
        }
    }

    /// The origin slot for an `http://` or `https://` URI, or `None` when the
    /// request must go through the pooled client (the system settings route
    /// it through a proxy, or the server picked HTTP/2).
    pub fn origin(&mut self, fast: &FastPath, uri: &Uri) -> Option<usize> {
        let https = uri.scheme() == Some(&Scheme::HTTPS);
        let authority = uri.authority()?.as_str();
        let i = match self.origins.iter().position(|o| o.https == https && o.authority == authority) {
            Some(i) => i,
            None => {
                // `Host` exactly as hyper-util's client sets it.
                let host = uri.host()?;
                let default_port = if https { 443 } else { 80 };
                let host = match uri.port_u16() {
                    Some(port) if port != default_port => HeaderValue::from_str(&format!("{host}:{port}")),
                    _ => HeaderValue::from_str(host),
                }
                .ok()?;
                let pooled = fast.proxy.intercept(uri).is_some() || (https && self.h2.lock().contains(authority));
                self.origins.push(Origin { https, authority: authority.to_string(), pooled, host, conn: None });
                self.origins.len() - 1
            }
        };
        (!self.origins[i].pooled).then_some(i)
    }

    /// Sends a request and waits for the response head. `headers` must
    /// already hold the client's default headers.
    pub async fn request(
        &mut self,
        origin: usize,
        method: Method,
        uri: &Uri,
        mut headers: HeaderMap,
        body: Option<Bytes>,
        now: Instant,
    ) -> Result<Head, Failure> {
        self.requests += 1;
        // Taken out while in flight: if the caller times out and drops this
        // future, the half-used connection is dropped with it.
        let o = &mut self.origins[origin];
        let mut conn = o.conn.take().filter(|c| c.io.is_some() && now.duration_since(c.last_used) < IDLE_TIMEOUT);
        let mut reused = conn.is_some();
        let mut c = match conn.take() {
            Some(c) => c,
            None => match self.connect(origin, uri, now).await? {
                Some(c) => c,
                None => {
                    self.requests -= 1;
                    return Err(Failure::Http2(Box::new(Unsent { method, headers, body })));
                }
            },
        };
        let o = &mut self.origins[origin];
        let added_host = !headers.contains_key(HOST);
        if added_host {
            headers.insert(HOST, o.host.clone());
        }
        let has_body = body.is_some();
        let mut req = Request::new(body.map_or_else(Body::default, Body::new));
        *req.method_mut() = method;
        *req.uri_mut() = origin_form(uri);
        *req.headers_mut() = headers;
        loop {
            let sent = c.sender.try_send_request(req);
            match drive(&mut c.io, sent).await {
                Ok(resp) => {
                    let (parts, body) = resp.into_parts();
                    c.last_used = now;
                    return Ok(Head { parts, body: BodyReader { body, conn: c, origin } });
                }
                // The server closed an idle connection before the request was
                // written: retry on a new one, like hyper-util's pool.
                Err(mut e) if reused && e.message().is_some() => {
                    req = e.take_message().unwrap();
                    reused = false;
                    match self.connect(origin, uri, now).await? {
                        Some(new) => c = new,
                        None => {
                            self.requests -= 1;
                            return Err(Failure::Http2(Box::new(unsent(req, added_host, has_body).await)));
                        }
                    }
                }
                Err(e) => return Err(Failure::Request(e.into_error())),
            }
        }
    }

    /// Opens a connection to the origin of `uri`, or `None` (and the origin
    /// goes to the pooled client) when its server picks HTTP/2.
    async fn connect(&mut self, origin: usize, uri: &Uri, now: Instant) -> Result<Option<Conn>, Failure> {
        let o = &mut self.origins[origin];
        if o.https && self.h2.lock().contains(&o.authority) {
            o.pooled = true;
            return Ok(None);
        }
        self.connections += 1;
        let conn = if o.https {
            let connecting = connect_tls(&mut self.connector, &self.tls, uri);
            match tokio::time::timeout(self.connect_timeout, connecting).await {
                Ok(Ok(Some(io))) => handshake(io, now).await?,
                Ok(Ok(None)) => {
                    o.pooled = true;
                    self.h2.lock().insert(o.authority.clone());
                    return Ok(None);
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err(Failure::ConnectTimeout),
            }
        } else {
            match tokio::time::timeout(self.connect_timeout, self.connector.call(uri.clone())).await {
                Ok(Ok(io)) => handshake(io, now).await?,
                Ok(Err(e)) => return Err(Failure::Connect(Box::new(e))),
                Err(_) => return Err(Failure::ConnectTimeout),
            }
        };
        Ok(Some(conn))
    }
}

/// A response head; the body is still on the connection.
pub struct Head {
    pub parts: Parts,
    pub body: BodyReader,
}

pub struct BodyReader {
    body: Incoming,
    conn: Conn,
    origin: usize,
}

impl BodyReader {
    /// Reads the whole body and gives the connection back for the next request.
    pub async fn read(mut self, conns: &mut Conns) -> Result<Bytes, hyper::Error> {
        let body = drive(&mut self.conn.io, self.body.collect()).await?.to_bytes();
        if self.conn.io.is_some() {
            conns.origins[self.origin].conn = Some(self.conn);
        }
        Ok(body)
    }
}

/// A request that was not sent, as it was passed to `Conns::request`.
async fn unsent(req: Request<Body>, added_host: bool, has_body: bool) -> Unsent {
    let (mut parts, body) = req.into_parts();
    if added_host {
        parts.headers.remove(HOST);
    }
    let body = body.collect().await.map(|b| b.to_bytes()).unwrap_or_default();
    Unsent { method: parts.method, headers: parts.headers, body: has_body.then_some(body) }
}

/// TCP and TLS connect as hyper-rustls does it for reqwest, with the same
/// errors; `None` when the server picked HTTP/2.
async fn connect_tls(
    connector: &mut HttpConnector,
    tls: &TlsConnector,
    uri: &Uri,
) -> Result<Option<TokioIo<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>>, Failure> {
    let name = server_name(uri).map_err(|e| Failure::Connect(Box::new(e)))?;
    let tcp = connector.call(uri.clone()).await.map_err(|e| Failure::Connect(Box::new(e)))?;
    let stream =
        tls.connect(name, tcp.into_inner()).await.map_err(|e| Failure::Connect(Box::new(std::io::Error::other(e))))?;
    Ok((stream.get_ref().1.alpn_protocol() != Some(b"h2")).then(|| TokioIo::new(stream)))
}

/// The TLS server name for a URI: its host, without IPv6 brackets.
fn server_name(uri: &Uri) -> Result<ServerName<'static>, rustls::pki_types::InvalidDnsNameError> {
    let host = uri.host().unwrap_or_default();
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    ServerName::try_from(host.to_string())
}

async fn handshake<T>(io: T, now: Instant) -> Result<Conn, Failure>
where
    T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let (sender, io) = http1::Builder::new().handshake(io).await.map_err(Failure::Request)?;
    Ok(Conn { sender, io: Some(Box::pin(io)), last_used: now })
}

/// Polls `fut` and the connection together, so the request and response I/O
/// happen in the calling task. Once the connection shuts down it is dropped;
/// errors reach `fut` through hyper's channels.
async fn drive<F: Future>(io: &mut Option<Driver>, fut: F) -> F::Output {
    let mut fut = pin!(fut);
    poll_fn(|cx| {
        if let Some(conn) = io
            && conn.as_mut().poll(cx).is_ready()
        {
            *io = None;
        }
        fut.as_mut().poll(cx)
    })
    .await
}

/// `/path?query` request target, as hyper-util's client sends it.
fn origin_form(uri: &Uri) -> Uri {
    match uri.path_and_query() {
        Some(pq) if pq.as_str() != "/" => {
            let mut parts = hyper::http::uri::Parts::default();
            parts.path_and_query = Some(pq.clone());
            Uri::from_parts(parts).expect("path is a valid URI")
        }
        _ => Uri::default(),
    }
}

#[derive(Clone, Copy)]
pub enum Encoding {
    Gzip,
    Deflate,
    Brotli,
    Zstd,
}

/// Picks the decoder for the response's `Content-Encoding` and drops that
/// header and `Content-Length`, as tower-http's decompression layer does with
/// the encodings reqwest enables.
pub fn take_encoding(headers: &mut HeaderMap) -> Option<Encoding> {
    let encoding = match headers.get(CONTENT_ENCODING)?.as_bytes() {
        b"gzip" => Encoding::Gzip,
        b"deflate" => Encoding::Deflate,
        b"br" => Encoding::Brotli,
        b"zstd" => Encoding::Zstd,
        _ => return None,
    };
    headers.remove(CONTENT_ENCODING);
    headers.remove(CONTENT_LENGTH);
    Some(encoding)
}

/// Decodes a whole body with the same decoders as that layer.
pub async fn decode(encoding: Option<Encoding>, body: Bytes) -> std::io::Result<Bytes> {
    use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
    use tokio::io::AsyncReadExt;

    // An empty body is passed through, not reported as truncated.
    let Some(encoding) = encoding.filter(|_| !body.is_empty()) else { return Ok(body) };
    let mut out = Vec::new();
    let input = &body[..];
    match encoding {
        Encoding::Gzip => GzipDecoder::new(input).read_to_end(&mut out).await?,
        Encoding::Deflate => ZlibDecoder::new(input).read_to_end(&mut out).await?,
        Encoding::Brotli => BrotliDecoder::new(input).read_to_end(&mut out).await?,
        Encoding::Zstd => {
            let mut decoder = ZstdDecoder::new(input);
            decoder.multiple_members(true);
            decoder.read_to_end(&mut out).await?
        }
    };
    Ok(out.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast(proxy: Matcher) -> FastPath {
        FastPath { proxy, ..FastPath::new(Duration::from_secs(1), false).unwrap() }
    }

    #[test]
    fn proxied_origins_use_the_pooled_client() {
        let uri: Uri = "http://api.test:8080/a".parse().unwrap();
        let direct = fast(Matcher::builder().build());
        let proxied = fast(Matcher::builder().http("http://proxy.test:3128").no("other.test").build());
        assert_eq!(Conns::new(&direct).origin(&direct, &uri), Some(0));
        assert_eq!(Conns::new(&proxied).origin(&proxied, &uri), None);
        let other: Uri = "http://other.test/".parse().unwrap();
        assert_eq!(Conns::new(&proxied).origin(&proxied, &other), Some(0));
    }

    #[test]
    fn host_header_and_request_target() {
        let f = fast(Matcher::builder().build());
        let mut conns = Conns::new(&f);
        let a: Uri = "http://api.test:8080/a?b=c".parse().unwrap();
        let b: Uri = "http://api.test/".parse().unwrap();
        assert_eq!(conns.origin(&f, &a), Some(0));
        assert_eq!(conns.origin(&f, &b), Some(1));
        assert_eq!(conns.origin(&f, &"http://api.test:8080/other".parse().unwrap()), Some(0));
        assert_eq!(conns.origins[0].host, "api.test:8080");
        assert_eq!(conns.origins[1].host, "api.test");
        assert_eq!(origin_form(&a), "/a?b=c");
        assert_eq!(origin_form(&b), "/");
        // Same authority over TLS is another origin, with 443 as the default port.
        let c: Uri = "https://api.test/".parse().unwrap();
        let d: Uri = "https://api.test:80/".parse().unwrap();
        assert_eq!(conns.origin(&f, &c), Some(2));
        assert_eq!(conns.origin(&f, &d), Some(3));
        assert_eq!(conns.origins[2].host, "api.test");
        assert_eq!(conns.origins[3].host, "api.test:80");
    }

    #[test]
    fn http2_origins_use_the_pooled_client() {
        let f = fast(Matcher::builder().build());
        f.h2.lock().insert("api.test".into());
        let mut conns = Conns::new(&f);
        assert_eq!(conns.origin(&f, &"https://api.test/".parse().unwrap()), None);
        assert_eq!(conns.origin(&f, &"http://api.test/".parse().unwrap()), Some(1));
        assert_eq!(conns.origin(&f, &"https://api.test:8443/".parse().unwrap()), Some(2));
    }

    #[test]
    fn tls_server_names() {
        let name = |u: &str| server_name(&u.parse().unwrap()).map(|n| n.to_str().into_owned());
        assert_eq!(name("https://api.test:8443/").unwrap(), "api.test");
        assert_eq!(name("https://127.0.0.1/").unwrap(), "127.0.0.1");
        assert_eq!(name("https://[::1]:8443/").unwrap(), "::1");
    }

    #[tokio::test]
    async fn decoding_follows_the_decompression_layer() {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        h.insert(CONTENT_LENGTH, HeaderValue::from_static("0"));
        let enc = take_encoding(&mut h);
        assert!(h.is_empty());
        // An empty body (HEAD, 204) is not a decoding error.
        assert_eq!(decode(enc, Bytes::new()).await.unwrap(), Bytes::new());
        assert!(decode(enc, Bytes::from_static(b"not gzip")).await.is_err());
        // Unknown or stacked encodings are passed through untouched.
        let mut h = HeaderMap::new();
        h.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip, br"));
        assert!(take_encoding(&mut h).is_none());
        assert_eq!(h.len(), 1);
    }
}
