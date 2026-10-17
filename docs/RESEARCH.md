# How loadman's behaviour was researched

loadman is an independent, clean-room implementation. We wanted it to *feel*
familiar to people who use the Performance tab of the Postman desktop app:
same load-profile shapes, same metric definitions, same data-file
distributions. To learn how that feature behaves, we inspected a locally
installed copy of the Postman desktop app with
[REA (Reverse Engineer Anything)](https://github.com/morluto/rea), a static
analysis tool for JavaScript/Electron applications, and wrote down the
**behaviour** we observed. No Postman code, assets or binaries are included
in this repository. loadman is not affiliated with or endorsed by Postman, Inc.

The collection, environment and globals file formats loadman reads are the
publicly documented [Postman Collection format](https://schema.postman.com).

## Target and tooling

| | |
|---|---|
| Target | Postman desktop app 12.32.4 (macOS, arm64), `Contents/Resources/app.asar` |
| asar SHA-256 | `30abb7db2af633bf854f32b60fde0a1dc3e8241140cc8f9e033ee6c1091564f4` |
| Tool | REA 6.5.0, built from source (`npm ci && npm run build:cached`) |
| Mode | Static only. REA parses JavaScript as inert text; nothing was executed. |

All work happened on a copy of the archive in a scratch directory; the
installed app was not modified.

## Steps

1. **Extract the archive** — `rea extract-artifact app.asar`
   (evidence `ev_c3650170de6d…a843`). 618 inventoried files.
2. **Locate the feature.** A keyword sweep (`loadProfile`, `virtualUsers`,
   `rampUp`, `performanceTest`) pointed at `perftest.js`, the renderer chunk
   `perftest-*.min.js`, IPC handlers, a `@postman/performance-test` data
   sink, a DuckDB metrics service and the VU execution worker.
3. **Build application graphs for just those modules** —
   `rea analyze-javascript-application <dir> --source-path <file>…`
   - `ev_90ed5fac1f43…fe66`: `perftest.js`, renderer chunk, IPC handlers
     (25,868 semantic nodes, 1,292 functions)
   - `ev_fe0f70ccfd91…f044`: data sink, process manager, execution handler
   - `ev_d2a5db4cec49…c85d`: DuckDB metrics service
   - `ev_80a739f60cd0…7f08`: VU execution process
4. **Read behaviour, not code.** Using the semantic graph's function and
   binding nodes we mapped hits back to their enclosing functions and
   resolved minified bindings (for example the scheduler's `lp(...)` is the
   exported `getTargetVUCount`).

## Findings that shaped loadman

### Load profiles

- A profile is a list of **control points** `(time, VU count)` plus a total
  duration. Four named types exist: `fixed`, `ramp-up`, `spike`, `peak`.
- The target VU count at elapsed time *t* is the **linear interpolation**
  between the neighbouring control points, **rounded** to a whole user; before
  the first point it is the first value, after the duration the last value.
  → `LoadProfile::target_vus` (`src/profile.rs`).
- The profile editor builds the shapes from fractions of the duration and of
  the maximum VU count:

  | type | base load | control points (time fraction → load) |
  |---|---|---|
  | ramp-up | 25 % | 0 → base, 0.25 → base, 0.5 → max, 1 → max |
  | spike | 10 % | 0 → base, 0.4 → base, 0.5 → max, 0.6 → base, 1 → base |
  | peak | 20 % | 0 → base, 0.2 → base, 0.4 → max, 0.6 → max, 0.8 → base, 1 → base |

  → `ProfileKind::shape`.
- Profiles are validated for strictly ordered control points within the total
  duration.

### Scheduling virtual users

- A timer fires every **1 s** (default), recomputes the target and sends
  "add users" / "remove users" to the worker. → engine tick (`--tick`).
- New VUs take the next index; removal stops the **highest-numbered** VUs
  first (LIFO).
- Each VU runs the selected requests **sequentially**, then starts over, until
  it is removed or the run ends ("each executing all requests sequentially").
- A watchdog samples CPU and memory, warning at 75 % and treating 95 % as
  critical. loadman does not throttle itself; its overhead is low enough
  that the machine running it is rarely the bottleneck (see BENCHMARKS.md).

### Data files

- Four distribution types: `ordered`, `random`, `sequential`, `round-robin`.
- A row is chosen **when a VU starts** and kept for that VU: ordered → row =
  VU index (no row beyond the file), sequential → VU index modulo rows,
  round-robin → a shared counter modulo rows, random → any row.
  → `DataSet::pick`. loadman adds an opt-in `--data-per-iteration` mode.

### Metrics

Per-request executions are stored as rows (request id/name, method, response
time, VU id/count, error flag and name, HTTP status, request/response sizes)
and aggregated with SQL:

- Response time: avg, min, max, **p90, p95, p99**, overall, per request and
  per time window.
- Throughput: requests per window divided by the window length.
- **Error rate** = (transport errors + executions with a script error) /
  executions. An HTTP 4xx/5xx response is *not* an error by itself; it only
  counts when a test fails. loadman uses the same definition and reports
  HTTP 4xx/5xx and failed tests as separate rates
  (`error_rate`, `http_error_rate`, `assertion_failure_rate`).
- Failed assertions: share of executions with at least one failed `pm.test`.
- Error samples are de-duplicated by request, error name and a hash of
  method, URL and status.

loadman computes the same figures from HDR histograms instead of storing
every execution, which keeps memory flat for long runs.

### Script assertions

The bundled chai-postman plugin treats `pm.response.to.be.ok` as status
**200** (reason "OK"), `success` as any 2XX and `error` as 4XX or 5XX. loadman's
own assertion layer (`src/js/prelude.js`) follows these semantics.

## What we did not do

- No Postman source, minified code, assets or binaries are copied or
  redistributed. The JavaScript sandbox, assertion library, metrics, report
  and scheduler in this repository were written from scratch.
- No license check, account check or usage limit of the Postman app was
  inspected, bypassed or modified. loadman does not interact with Postman
  services at all; it reads exported JSON files.
- Nothing was executed from the inspected application.

If you maintain a product and believe something here goes beyond describing
observable behaviour, please open an issue.
