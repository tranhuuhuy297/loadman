//! Local benchmark target: `cargo run --release --example target_server -- 127.0.0.1:8080`
//!
//! Optional arguments: worker threads (default 4), then `tls` for HTTPS with
//! HTTP/1.1 or `tls-h2` for HTTPS offering HTTP/2 too (self-signed
//! certificate: run loadman with `--insecure`).

#[path = "../tests/support/server.rs"]
mod server;

fn main() {
    let addr = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:8080".into());
    let threads: usize = std::env::args().nth(2).and_then(|t| t.parse().ok()).unwrap_or(4);
    let alpn: Option<&[&str]> = match std::env::args().nth(3).as_deref() {
        Some("tls") => Some(&["http/1.1"]),
        Some("tls-h2") => Some(&["h2", "http/1.1"]),
        _ => None,
    };
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(threads).enable_all().build().unwrap();
    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
        let scheme = if alpn.is_some() { "https" } else { "http" };
        eprintln!("target server listening on {scheme}://{addr} ({threads} threads)");
        match alpn {
            Some(alpn) => server::serve_tls(listener, server::tls_acceptor(alpn)).await,
            None => server::serve(listener).await,
        }
    });
}
