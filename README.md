# loadman

**Load-test your Postman collections from the terminal or CI.** Ramp-up,
spike and peak profiles, `pm.test` scripts, pass/fail thresholds, JSON and
HTML reports. One 7 MB binary, no account, no VU-hour bill.

```bash
loadman run shop.postman_collection.json -e staging.json \
  --profile ramp-up --vus 200 --duration 5m \
  --threshold "p95<300ms" --threshold "error_rate<1%" \
  --html report.html
```

![HTML report](docs/report.png)

## Why

The Postman app has a Performance tab, but it runs inside the desktop app.
Running performance tests headless in the cloud bills
[virtual-user hours](https://learning.postman.com/docs/billing/resource-usage)
on paid plans. Teams that already keep their API flows in Postman collections
end up rewriting them for k6 or JMeter just to run load tests in CI.

loadman runs the collections you already have, as they are:

- **The model Postman documents for performance tests.** Fixed, ramp-up,
  spike and peak load profiles; VUs that run the requests in order, in a loop;
  ordered and randomized data files; the same error-rate definition
  ([compatibility notes](docs/COMPATIBILITY.md)).
- **Your scripts keep working.** Pre-request and test scripts run in embedded
  QuickJS with a `pm.*` API: chaining a login token into later requests,
  `pm.test`/`pm.expect` assertions, `pm.response.to.have.status(...)`.
- **Built for CI.** Thresholds turn a run into a pass/fail exit code. The JSON
  report holds every number, including per-second history.
- **Cheap to run.** Rust, thread-per-core, HDR histograms. It sustains
  ~190k req/s from a laptop, and with scripts on it needs ~57× less CPU per
  request than `postman-runtime` ([benchmarks](docs/BENCHMARKS.md)).

## Install

```bash
# Homebrew (macOS and Linux)
brew install tranhuuhuy297/tap/loadman

# Docker (linux/amd64 and linux/arm64)
docker run --rm -v "$PWD:/work" ghcr.io/tranhuuhuy297/loadman run collection.json -u 20 -t 1m

# From source
cargo install --git https://github.com/tranhuuhuy297/loadman
```

Prebuilt binaries for Linux (x86_64 and arm64, glibc 2.28 or newer), macOS
(Apple Silicon and Intel) and Windows are attached to each
[release](https://github.com/tranhuuhuy297/loadman/releases). They need no
system CA bundle: without one, `https://` is verified with bundled Mozilla
roots.

## Usage

```bash
# What will run? Lists requests, auth, bodies, scripts and missing variables.
loadman inspect collection.json

# Preview a load profile without sending traffic.
loadman profile --profile peak --vus 500 --duration 10m

# 50 VUs for 2 minutes with an environment and a data file.
loadman run collection.json -e env.json -d users.csv -u 50 -t 2m

# Only one folder, with think time between requests.
loadman run collection.json --folder Checkout --think-time 200ms-1s
```

Try it locally against the bundled demo API:

```bash
cargo run --release --example target_server &
loadman run examples/demo/collection.json -e examples/demo/environment.json \
  -d examples/demo/users.csv -p spike -u 300 -t 30s --html report.html
```

### Load profiles

| `--profile` | shape (fractions of `--duration`) | default base load |
|---|---|---|
| `fixed` | `--vus` for the whole run | — |
| `ramp-up` | base until 25 %, linear to max by 50 %, hold | 25 % of `--vus` |
| `spike` | base until 40 %, max at 50 %, back to base at 60 % | 10 % |
| `peak` | base until 20 %, max 40–60 %, base again from 80 % | 20 % |

Override the base with `--base-vus`, draw any shape with
`--stages "0:10,1m:100,4m:100,5m:0"` (absolute `time:VUs` points, linear in
between), or pass a Postman-style load-profile JSON with `--load-profile`.
The VU count is adjusted once per second; each VU runs the selected requests
in order and loops until it is stopped.

`loadman profile ...` prints the shape without sending anything.

### Arrival rate (open model)

With VUs, a slow server gets fewer requests: each VU waits for its
responses. `--rate` instead starts iterations on a schedule, whether or not
earlier ones have finished, which is how real users arrive:

```bash
# 200 iterations per second for 5 minutes.
loadman run api.json --rate 200 -t 5m
# Ramp from 50/s to 500/s: the profile's shape, with its peak at --rate.
loadman run api.json -p ramp-up --rate 500 -t 10m --max-vus 2000
```

The load profile keeps its shape and its values become rates: the peak is
`--rate` iterations per second and every other point scales with it (with
`--stages "0:0,1m:10,5m:10"` and `--rate 300`, the rate climbs from 0 to 300/s
in the first minute). Rates change smoothly between points. Each iteration
goes to an idle VU, or a new one while fewer than `--max-vus` (default 1000)
exist; when all of them are busy the iteration is dropped and counted.
`--threshold dropped_iterations<1` fails the run if that happens.

### Thresholds and exit codes

`--threshold` takes `metric op value`, e.g. `p95<300ms`, `avg<=0.2s`,
`error_rate<1%`, `rps>=500`. Metrics: `avg min max p50 p90 p95 p99 rps
error_rate http_error_rate assertion_failure_rate requests iterations
dropped_iterations`.

Exit code `0`: all thresholds passed. `1`: at least one failed. `2`: invalid
input (bad collection, file not found, ...).

In GitHub Actions, the repository's action installs the release binary for
the runner (Linux, macOS or Windows) and runs it; `args` is split on
whitespace and not evaluated by a shell, so thresholds need no quoting:

```yaml
# .github/workflows/load.yml
- uses: tranhuuhuy297/loadman@v0.2.0
  with:
    collection: api.postman_collection.json
    args: -e ci.json -u 100 -t 3m --threshold p95<400ms --threshold error_rate<0.5% --out load.json --html load.html
- uses: actions/upload-artifact@v4
  if: always()
  with: { name: load-report, path: "load.*" }
```

### Metrics

| metric | meaning |
|---|---|
| response time | send → full body received, ms; avg/min/max/p50/p90/p95/p99 |
| `error_rate` | requests with a transport error (refused, timeout, DNS, TLS, reset) **or** a script error. A 4xx/5xx is not an error by itself, as in Postman |
| `http_error_rate` | responses with status ≥ 400 |
| `assertion_failure_rate` | requests with at least one failed `pm.test` |
| `rps` | completed requests per second over the run |

Percentiles come from HDR histograms (±0.1 % for totals, ±1 % per second).

### Prometheus

`--prometheus-listen 127.0.0.1:9464` serves live metrics at `/metrics` while
the test runs (use `0.0.0.0:9464` inside a container), and
`--prometheus-push http://pushgateway:9091` pushes them to a Pushgateway every
5 seconds and once more when the test ends, so short CI runs are not lost
between scrapes. The push goes to job `loadman` unless the URL names one
(`.../metrics/job/ci/run/42`).

| metric | type | labels |
|---|---|---|
| `loadman_vus`, `loadman_elapsed_seconds` | gauge | |
| `loadman_iterations_total` | counter | |
| `loadman_requests_total`, `loadman_errors_total`, `loadman_transport_errors_total`, `loadman_script_errors_total`, `loadman_http_errors_total`, `loadman_assertion_failures_total`, `loadman_received_bytes_total`, `loadman_sent_bytes_total` | counter | `request` (folder path and name), `index` |
| `loadman_response_time_seconds` | summary (p50/p90/p95/p99 since the start, `_sum`, `_count`) | `request`, `index` |
| `loadman_responses_total` | counter | `status` |
| `loadman_errors_by_code_total` | counter | `code` (`ETIMEDOUT`, `ECONNREFUSED`, ...) |
| `loadman_info` | gauge | `collection`, `version` |

For throughput, use `rate(loadman_requests_total[30s])`.

### Scripts: what is supported

| | |
|---|---|
| Variables | `pm.environment`, `pm.collectionVariables`, `pm.globals`, `pm.variables`, `pm.iterationData` (`get/set/unset/has/clear/toObject/replaceIn`) |
| Request | `pm.request.url/method/headers` (headers can be changed in pre-request scripts), `pm.execution.skipRequest()` |
| Flow | `pm.execution.setNextRequest(name)` (also `postman.setNextRequest`) jumps to a request by name, folder path (`Folder / Request`) or item id after the current one; `null` ends the iteration |
| Response | `code`, `status`, `responseTime`, `responseSize`, `headers`, `text()`, `json()` |
| Assertions | `pm.test`, `pm.expect` with the common chai chain (`equal/eql/include/property/a/an/above/below/match/lengthOf/keys/oneOf/...`), `pm.response.to.have.status/header/body/jsonBody`, `to.be.ok/success/error/clientError/serverError/json/...` |
| Legacy | `tests[...]`, `responseBody`, `responseCode`, `responseTime`, `postman.setEnvironmentVariable(...)` |
| Requests from scripts | `pm.sendRequest(req, callback)` and `await pm.sendRequest(req)`; `req` is a URL or `{url, method, header, body}` with raw, urlencoded, GraphQL, form-data (text and file fields) or file bodies; file paths are relative to the collection |
| Modules | `require('crypto-js')`, `require('lodash')` (also as `_`), `require('moment')`, `require('uuid')`, `require('chai')` |
| Cookies | a jar per VU: `Set-Cookie` is stored (redirects included) and sent back; `pm.cookies.get/has/toObject`, `pm.cookies.jar().set/unset`; `--no-cookies` turns it off |
| Async | top-level `await`, async `pm.test` functions and `done` callbacks; `setTimeout` callbacks run as soon as the script yields |
| Misc | `btoa/atob`, `console.*` (shown with `--verbose`), script timeout (`--script-timeout`) |

Not yet: `pm.require` package libraries and JSON schema assertions. Calling
them records a script error, so nothing is skipped silently.

`pm.sendRequest` waits for the response on the VU's script engine and is not
counted in the request metrics; it uses the run's timeout and `--insecure`,
within the script's own time limit. Its requests do not use the VU's cookie
jar.

Each VU keeps its own copy of the variables, seeded from the collection,
environment, globals, `--env-var` and its data row, and its own cookie jar.
Changes made by one VU's scripts are not visible to other VUs.

The bundled libraries are unmodified copies under the MIT license; see
[src/js/vendor/LICENSES.md](src/js/vendor/LICENSES.md).

### Requests

Collections v2.0/v2.1, nested folders, `{{variables}}` (nested, plus dynamic
ones such as `{{$guid}}`, `{{$randomInt}}`, `{{$timestamp}}`), path
variables, auth inherited from collection and folders (bearer, basic, API
key, OAuth 2 with a stored token), raw/JSON, urlencoded, form-data
(including files), binary and GraphQL bodies, redirects, gzip/brotli/zstd.

`http://` and `https://` requests go over HTTP/1.1 connections that each VU
owns and drives itself, which is cheaper per request than a shared pool. TLS
offers HTTP/2 and HTTP/1.1 like the pooled client does; servers that pick
HTTP/2, proxied requests (`HTTP_PROXY` and friends) and the hops after a
redirect use the pooled client, so HTTP/2 servers still get HTTP/2.
`--no-fast-path` sends everything through the pool.

## Roadmap

- Distributed mode: one controller, many agents, merged report
- OpenTelemetry (OTLP) export

## Compatibility

loadman reads the publicly documented
[Postman Collection format](https://schema.postman.com) and follows the
performance-testing behaviour described in Postman's public documentation;
details and sources are in [docs/COMPATIBILITY.md](docs/COMPATIBILITY.md).
It is an independent implementation that contains no Postman code and does not
talk to Postman services. Postman is a trademark of Postman, Inc.; loadman is
not affiliated with or endorsed by Postman, Inc.

## License

MIT OR Apache-2.0, at your option.
