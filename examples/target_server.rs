//! Local benchmark target: `cargo run --release --example target_server -- 127.0.0.1:8080`

#[path = "../tests/support/server.rs"]
mod server;

fn main() {
    let addr = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:8080".into());
    let threads: usize = std::env::args().nth(2).and_then(|t| t.parse().ok()).unwrap_or(4);
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(threads).enable_all().build().unwrap();
    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
        eprintln!("target server listening on http://{addr} ({threads} threads)");
        server::serve(listener).await;
    });
}
