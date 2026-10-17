# Benchmarks

Reproduce with `./bench/compare.sh` (needs cargo, Node.js ≥ 22 and npm).
Everything runs on **one machine**: the bundled Rust target server
(`examples/target_server.rs`, 4 threads, tiny responses) and the load
generator compete for the same cores, so absolute numbers are a property of
this setup, not of a real API. Use them to compare engines, not to predict
your service's capacity.

Machine: Apple M5, 10 cores, 24 GB RAM, macOS 26.4. loadman 0.1.0 (release,
LTO), Node.js 26.12, newman 6.2.3. Run on 2026-10-17.

## A. Raw HTTP engine: 100 VUs, one `GET /hello`, no scripts, 15 s

| engine | requests/s | p95 | peak RSS | CPU per request |
|---|---:|---:|---:|---:|
| **loadman** (`--no-scripts`, default 10 workers) | **171,148** | 1.0 ms | **17 MB** | 26 µs |
| Node.js, hand-written closed loop (`bench/node-baseline.mjs`) | 92,237 | 1.31 ms | 189 MB | 11 µs |

The Node baseline is deliberately minimal: a keep-alive `http.Agent`, no
variables, no scripts, latencies pushed into an array. It is a lower bound
for any Node-based engine, not a real tool.

Honest reading: loadman delivers 1.9× the throughput at a tenth of the
memory because it uses every core, but on this shared machine it spends more
CPU per request than the single-threaded loop. Most of that is kernel time
(`writev`/`recvfrom`/`kevent`) and cross-core contention with the target
server; with `--workers 1` loadman's CPU per request is 11.8 µs, on par with
the baseline, and its user-space time per request (≈6.5 µs) is already below
Node's (≈7.6 µs). Cutting syscalls further (one connection driven inside each
VU's task instead of hyper's pooled dispatch) is on the roadmap.

## B. Collections with test scripts: 1 VU, the demo collection

The demo collection (`examples/demo`) logs in, stores the token from the
response with `pm.environment.set`, calls an authenticated endpoint and a
health check. Each request runs `pm.test` assertions (5 per iteration).

| engine | requests/s per VU | CPU per request | peak RSS |
|---|---:|---:|---:|
| newman 6.2.3 (`postman-runtime`, 1,000 iterations) | 101 | 2.7 ms | 336 MB |
| **loadman** (`-u 1 --workers 1 --script-threads 1`, 10 s) | **16,276** | **0.048 ms** | **13 MB** |

newman is Postman's open-source collection runner. It executes collections on
`postman-runtime`, the same runtime package the desktop app's performance
worker bundles, so it is the closest headless stand-in for the cost of running
a request plus its scripts there. Per request, loadman uses about 57× less
CPU and 26× less memory, and one loadman VU runs as fast as ~160 newman VUs.
(newman is a functional runner, not a load tester; this measures per-request
engine overhead, not newman's intended use.)

## C. Scaling with scripts

Demo collection, ramp-up profile up to 100 VUs, 20 s, `pm.test` on every
response, data file attached: 1,711,056 requests (85,548 req/s), 2,851,754
assertions, 0 errors, p95 0.33 ms, peak RSS 27 MB. Script execution is the
dominant cost here (121 s user CPU across 5 script threads and 10 I/O
workers).
