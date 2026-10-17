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
| **loadman** (`--no-scripts`, default 10 workers) | **194,745** | 0.82 ms | **16 MB** | 21.4 µs |
| loadman before per-VU connections (pooled client only) | 183,933 | 0.87 ms | 18 MB | 28.0 µs |
| Node.js, hand-written closed loop (`bench/node-baseline.mjs`) | 92,984 | 1.33 ms | 190 MB | 11.1 µs |

The Node baseline is deliberately minimal: a keep-alive `http.Agent`, no
variables, no scripts, latencies pushed into an array. It is a lower bound
for any Node-based engine, not a real tool.

Plain `http://` requests now go over HTTP/1.1 connections that each VU owns
and drives from its own task, instead of a pooled client that hands every
request to a separate connection task; the VU loop also stopped yielding to
the scheduler after every iteration, which cost an extra `kevent` call per
request. Instructions per request fell from about 100k to 60k and user CPU
per request roughly halved. These runs shared the machine with an unrelated
process using about one core; over four rounds, CPU per request was
21.3–23.3 µs against 27.7–29.7 µs for the previous engine.

The same holds over TLS. With the target serving `https://` and offering only
HTTP/1.1 (`target_server 127.0.0.1:8443 4 tls`, loadman with `--insecure`),
three interleaved rounds gave 23.8, 18.0 and 18.0 µs CPU per request on per-VU
connections against 29.1, 24.0 and 24.1 µs on the pooled client, with user
CPU per request roughly halved and equal or higher throughput (187–189k req/s
on a quiet machine). Servers that pick HTTP/2 stay on the pooled client.

Honest reading: loadman delivers 2× the throughput at a tenth of the memory
because it uses every core, but on this shared machine it still spends about
twice the CPU per request of the single-threaded loop. The rest is kernel
time: with ten worker threads next to the target's four on a chip with 4
performance and 6 efficiency cores, each `writev`/`recvfrom` costs several
times more than with one thread, and workers sleep and wake for almost every
response. With `--workers 1` loadman sends 160–190k req/s from one thread at
5–6 µs CPU per request, about half the Node loop's CPU per request at nearly
twice its throughput; `--workers 2` gives about 178k req/s at 11 µs.

## B. Collections with test scripts: 1 VU, the demo collection

The demo collection (`examples/demo`) logs in, stores the token from the
response with `pm.environment.set`, calls an authenticated endpoint and a
health check. Each request runs `pm.test` assertions (5 per iteration).

| engine | requests/s per VU | CPU per request | peak RSS |
|---|---:|---:|---:|
| newman 6.2.3 (`postman-runtime`, 1,000 iterations) | 101 | 2.7 ms | 336 MB |
| **loadman** (`-u 1 --workers 1 --script-threads 1`, 10 s) | **16,276** | **0.048 ms** | **13 MB** |

newman is Postman's open-source collection runner, built on the open-source
`postman-runtime` package, so it shows what running a request plus its
scripts costs in Postman's own runtime. Per request, loadman uses about 57× less
CPU and 26× less memory, and one loadman VU runs as fast as ~160 newman VUs.
(newman is a functional runner, not a load tester; this measures per-request
engine overhead, not newman's intended use.)

## C. Scaling with scripts

Demo collection, ramp-up profile up to 100 VUs, 20 s, `pm.test` on every
response, data file attached: 1,711,056 requests (85,548 req/s), 2,851,754
assertions, 0 errors, p95 0.33 ms, peak RSS 27 MB. Script execution is the
dominant cost here (121 s user CPU across 5 script threads and 10 I/O
workers).
