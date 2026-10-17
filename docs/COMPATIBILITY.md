# Compatibility with Postman collections

loadman reads the publicly documented
[Postman Collection format](https://schema.postman.com) (v2.0 and v2.1) and
environment/globals exports, and follows the performance-testing model that
Postman describes in its public documentation. It is an independent
implementation: it contains no Postman code and never talks to Postman
services. Postman is a trademark of Postman, Inc.; loadman is not affiliated
with or endorsed by Postman, Inc.

Sources:
[Configure and run performance tests](https://learning.postman.com/docs/tests-and-scripts/performance-testing/performance-test-configuration),
[Use a data file to simulate virtual users](https://learning.postman.com/docs/tests-and-scripts/performance-testing/test-data/performance-test-data-files).

## Virtual users

As documented by Postman, each virtual user runs the selected requests in
order and then repeats that sequence in a loop for the duration of the test;
all virtual users run in parallel. loadman does the same. `--folder` and
`--request` select which requests run.

The VU count is adjusted to the load profile once per second (`--tick`).
Added VUs get the next index; when the count drops, the newest VUs stop
first, after finishing the request they are executing.

## Load profiles

The public documentation describes four profiles, which loadman provides
under the same names:

| profile | documented behaviour | loadman's default shape |
|---|---|---|
| `fixed` | the maximum number of VUs runs for the whole test | `--vus` throughout |
| `ramp-up` | from an initial load up to the maximum over the ramp-up period | 25 % of `--vus` until 25 % of the duration, linear to the maximum at 50 %, then held |
| `spike` | from a base load up to the maximum and back to the base load | base 10 %, rising from 40 % of the duration to the maximum at 50 %, back to base at 60 % |
| `peak` | from a base load up to the maximum, held, then back to the base load | base 20 %, up between 20 % and 40 %, held until 60 %, down by 80 % |

The default fractions are loadman's own choices; change the base with
`--base-vus`, or describe any shape with `--stages "0:10,1m:100,4m:100,5m:0"`
(the VU count changes linearly between points and is rounded to whole users).
`--load-profile` reads a JSON description with `type`,
`options.totalDuration.millis` and `controlPoints[] {load.count, time.millis}`.

## Data files

CSV or JSON (an array of objects); values are available as `{{name}}` and
through `pm.iterationData`.

| `--data-distribution` | behaviour |
|---|---|
| `ordered` | as documented by Postman: VU n uses row n; extra rows are not used, and VUs beyond the last row run without data |
| `random` | as documented by Postman: a random row every time the VU starts the collection again |
| `sequential` | loadman extension: VU n uses row n modulo the number of rows |
| `round-robin` | loadman extension: rows are handed out in turn as VUs start |

`--data-per-iteration` makes `ordered`, `sequential` and `round-robin` pick a
new row on every iteration as well.

## Metrics

Following the definitions in Postman's documentation:

- **Response time**: average and the 90th, 95th and 99th percentiles, in
  milliseconds (loadman also reports min, max and the median). It is measured
  from sending the request to receiving the whole body.
- **Throughput**: requests per second.
- **Error rate** (`error_rate`): the share of requests that failed at run time,
  such as timeouts, connection or TLS failures, or an uncaught exception in a
  script. Failed test assertions are reported separately
  (`assertion_failure_rate`), and so are HTTP 4xx/5xx responses
  (`http_error_rate`), which are not errors by themselves.

Percentiles come from HDR histograms (within 0.1 % for totals, 1 % per second).

## Scripts

Pre-request and test scripts run for every request, as in Postman's
performance tests. See the README for the supported `pm.*` API and the
modules available through `require`.

Each VU keeps its own copy of the variables, seeded from the collection,
environment, globals, `--env-var` and its data row; changes made by one VU's
scripts are not visible to other VUs. Each VU also has its own cookie jar.
