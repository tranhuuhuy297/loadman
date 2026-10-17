//! Self-contained HTML report (no network access needed to open it).

use crate::report::Report;

const TEMPLATE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Load test report</title>
<style>
:root {
  color-scheme: light;
  --page: #f9f9f7; --surface: #fcfcfb; --ink: #0b0b0b; --ink-2: #52514e; --muted: #898781;
  --grid: #e1e0d9; --axis: #c3c2b7; --ring: rgba(11,11,11,0.10);
  --s1: #2a78d6; --s2: #eb6834; --s3: #1baf7a; --s4: #eda100;
  --good: #006300; --critical: #d03b3b;
}
@media (prefers-color-scheme: dark) {
  :root:where(:not([data-theme="light"])) {
    color-scheme: dark;
    --page: #0d0d0d; --surface: #1a1a19; --ink: #f0efec; --ink-2: #c3c2b7; --muted: #898781;
    --grid: #2c2c2a; --axis: #383835; --ring: rgba(255,255,255,0.10);
    --s1: #3987e5; --s2: #d95926; --s3: #199e70; --s4: #c98500;
    --good: #0ca30c; --critical: #e66767;
  }
}
:root[data-theme="dark"] {
  color-scheme: dark;
  --page: #0d0d0d; --surface: #1a1a19; --ink: #f0efec; --ink-2: #c3c2b7; --muted: #898781;
  --grid: #2c2c2a; --axis: #383835; --ring: rgba(255,255,255,0.10);
  --s1: #3987e5; --s2: #d95926; --s3: #199e70; --s4: #c98500;
  --good: #0ca30c; --critical: #e66767;
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--page); color: var(--ink); font: 14px/1.45 system-ui, -apple-system, "Segoe UI", sans-serif; }
main { max-width: 1120px; margin: 0 auto; padding: 32px 16px 64px; }
header h1 { font-size: 22px; margin: 0 0 4px; font-weight: 600; }
header p { margin: 0; color: var(--ink-2); }
.badge { display: inline-block; padding: 1px 8px; border-radius: 999px; font-size: 12px; font-weight: 600; margin-left: 8px; vertical-align: 2px; border: 1px solid var(--ring); }
.badge.pass { color: var(--good); } .badge.fail { color: var(--critical); }
.tiles { display: grid; grid-template-columns: repeat(auto-fit, minmax(150px, 1fr)); gap: 12px; margin: 24px 0; }
.tile, .card { background: var(--surface); border: 1px solid var(--ring); border-radius: 12px; }
.tile { padding: 14px 16px; }
.tile .label { color: var(--ink-2); font-size: 13px; }
.tile .value { font-size: 24px; font-weight: 600; margin-top: 2px; }
.tile .sub { color: var(--muted); font-size: 12px; }
.grid2 { display: grid; grid-template-columns: repeat(auto-fit, minmax(min(100%, 480px), 1fr)); gap: 12px; }
.card { padding: 16px; position: relative; min-width: 0; }
.card h2 { font-size: 15px; margin: 0; font-weight: 600; }
.card .desc { color: var(--ink-2); font-size: 13px; margin: 2px 0 8px; }
.legend { display: flex; flex-wrap: wrap; gap: 4px 14px; font-size: 12px; color: var(--ink-2); margin-bottom: 4px; }
.legend span::before { content: ""; display: inline-block; width: 14px; height: 2px; border-radius: 2px; background: var(--c); vertical-align: middle; margin-right: 6px; }
svg { display: block; width: 100%; height: auto; overflow: visible; }
svg text { fill: var(--muted); font-size: 11px; font-variant-numeric: tabular-nums; }
svg .end-label { fill: var(--ink-2); }
.tip { position: absolute; pointer-events: none; background: var(--surface); border: 1px solid var(--ring); border-radius: 8px; padding: 8px 10px; font-size: 12px; box-shadow: 0 4px 16px rgba(0,0,0,0.12); display: none; min-width: 140px; z-index: 2; }
.tip .t { color: var(--muted); margin-bottom: 4px; }
.tip .row { display: flex; align-items: center; gap: 8px; justify-content: space-between; }
.tip .row b { font-weight: 600; color: var(--ink); font-variant-numeric: tabular-nums; }
.tip .row span { color: var(--ink-2); }
.tip .row span::before { content: ""; display: inline-block; width: 10px; height: 2px; background: var(--c); vertical-align: middle; margin-right: 6px; }
section { margin-top: 28px; }
section > h2 { font-size: 16px; margin: 0 0 10px; font-weight: 600; }
.table-wrap { overflow-x: auto; background: var(--surface); border: 1px solid var(--ring); border-radius: 12px; }
table { border-collapse: collapse; width: 100%; font-size: 13px; }
th, td { text-align: right; padding: 8px 12px; border-bottom: 1px solid var(--grid); white-space: nowrap; font-variant-numeric: tabular-nums; }
th { color: var(--ink-2); font-weight: 600; background: var(--surface); }
th:first-child, td:first-child { text-align: left; white-space: normal; min-width: 180px; }
tr:last-child td { border-bottom: none; }
td.bad { color: var(--critical); font-weight: 600; }
.method { color: var(--muted); font-size: 11px; margin-right: 6px; }
details { margin-top: 12px; } summary { cursor: pointer; color: var(--ink-2); }
.warn { color: var(--ink-2); margin: 4px 0; } .warn::before { content: "! "; color: var(--critical); font-weight: 700; }
.mark-ok { color: var(--good); font-weight: 700; } .mark-bad { color: var(--critical); font-weight: 700; }
footer { margin-top: 40px; color: var(--muted); font-size: 12px; }
</style>
</head>
<body>
<main>
  <header>
    <h1 id="title"></h1>
    <p id="subtitle"></p>
  </header>
  <div class="tiles" id="tiles"></div>
  <div class="grid2">
    <div class="card"><h2>Virtual users</h2><p class="desc">Active VUs per second</p><div class="chart" id="c-vus"></div></div>
    <div class="card"><h2>Throughput</h2><p class="desc">Completed requests per second</p><div class="chart" id="c-rps"></div></div>
    <div class="card"><h2>Response time</h2><p class="desc">Per-second average and percentiles, ms</p><div class="chart" id="c-lat"></div></div>
    <div class="card"><h2>Error rate</h2><p class="desc">Transport and script errors, % of requests per second</p><div class="chart" id="c-err"></div></div>
  </div>
  <section><h2>Requests</h2><div class="table-wrap"><table id="t-req"></table></div></section>
  <section id="s-thr" hidden><h2>Thresholds</h2><div class="table-wrap"><table id="t-thr"></table></div></section>
  <section id="s-tests" hidden><h2>Tests</h2><div class="table-wrap"><table id="t-tests"></table></div></section>
  <section id="s-err" hidden><h2>Errors and status codes</h2><div class="table-wrap"><table id="t-err"></table></div></section>
  <section id="s-warn" hidden><h2>Warnings</h2><div id="warnings"></div></section>
  <section>
    <details><summary>Per-second data table</summary><div class="table-wrap" style="margin-top:8px"><table id="t-time"></table></div></details>
  </section>
  <footer id="footer"></footer>
</main>
<script id="data" type="application/json">__DATA__</script>
<script>
(() => {
  const R = JSON.parse(document.getElementById("data").textContent);
  const $ = (id) => document.getElementById(id);
  const el = (tag, attrs = {}, text) => {
    const e = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, v);
    if (text !== undefined) e.textContent = text;
    return e;
  };
  const NS = "http://www.w3.org/2000/svg";
  const sv = (tag, attrs = {}) => { const e = document.createElementNS(NS, tag); for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, v); return e; };
  const fmtN = (n) => n.toLocaleString("en-US");
  const fmtC = (n) => n >= 1e6 ? (n / 1e6).toFixed(1).replace(/\.0$/, "") + "M" : n >= 1e3 ? (n / 1e3).toFixed(1).replace(/\.0$/, "") + "K" : String(Math.round(n * 100) / 100);
  const fmtMs = (v) => v >= 1000 ? (v / 1000).toFixed(2) + " s" : v >= 100 ? v.toFixed(0) + " ms" : v >= 1 ? v.toFixed(1) + " ms" : v.toFixed(2) + " ms";
  const fmtB = (n) => { const u = ["B", "KB", "MB", "GB"]; let i = 0; while (n >= 1024 && i < 3) { n /= 1024; i++; } return (i ? n.toFixed(1) : n) + " " + u[i]; };
  const fmtT = (s) => s >= 60 ? Math.floor(s / 60) + "m" + String(s % 60).padStart(2, "0") + "s" : s + "s";
  const T = R.totals;

  document.title = R.collection + " · load test";
  $("title").textContent = R.collection;
  if (R.thresholds.length) {
    const ok = R.thresholds.every((t) => t.passed);
    $("title").append(el("span", { class: "badge " + (ok ? "pass" : "fail") }, ok ? "thresholds passed" : "thresholds failed"));
  }
  $("subtitle").textContent = `${R.profile.kind} profile · up to ${R.profile.max_vus} VUs · ${R.duration_s.toFixed(1)} s · started ${R.started_at}` + (R.interrupted ? " · interrupted" : "");

  const tiles = [
    ["Requests", fmtN(T.requests), T.rps.toFixed(1) + " req/s"],
    ["Average response", fmtMs(T.avg_ms), "min " + fmtMs(T.min_ms) + " · max " + fmtMs(T.max_ms)],
    ["95th percentile", fmtMs(T.p95_ms), "p90 " + fmtMs(T.p90_ms) + " · p99 " + fmtMs(T.p99_ms)],
    ["Error rate", T.error_rate.toFixed(2) + "%", fmtN(T.transport_errors + T.script_errors) + " requests"],
    ["Failed tests", T.assertion_failure_rate.toFixed(2) + "%", fmtN(T.failed_assertion_requests) + " requests"],
    ["Data received", fmtB(T.bytes_in), fmtB(T.bytes_out) + " sent · " + fmtN(R.iterations) + " iterations"],
  ];
  for (const [l, v, s] of tiles) {
    const t = el("div", { class: "tile" });
    t.append(el("div", { class: "label" }, l), el("div", { class: "value" }, v), el("div", { class: "sub" }, s));
    $("tiles").append(t);
  }

  // ------------------------------------------------------------ line charts
  const TL = R.timeline;
  // Round tick step (1, 2, 2.5, 5 x 10^k) giving at most five intervals, with a little headroom.
  function scale(v, cap) {
    if (!(v > 0)) return { step: 0.25, n: 4 };
    v = v * 1.05;
    const raw = v / 5, p = Math.pow(10, Math.floor(Math.log10(raw)));
    let step = 10 * p;
    for (const m of [1, 2, 2.5, 5, 10]) if (m * p >= raw) { step = m * p; break; }
    let n = Math.max(1, Math.ceil(v / step));
    if (cap && step * n > cap) n = Math.max(1, Math.floor(cap / step));
    return { step, n };
  }
  function chart(host, series, opt) {
    const card = host.parentElement;
    if (series.length > 1) {
      const lg = el("div", { class: "legend" });
      for (const s of series) { const sp = el("span", {}, s.name); sp.style.setProperty("--c", `var(${s.color})`); lg.append(sp); }
      host.before(lg);
    }
    const tip = el("div", { class: "tip", role: "status" });
    card.append(tip);
    const draw = () => {
      host.textContent = "";
      const W = Math.max(280, host.clientWidth), H = 200, m = { l: 44, r: series.length > 1 ? 40 : 12, t: 8, b: 22 };
      const iw = W - m.l - m.r, ih = H - m.t - m.b;
      const svg = sv("svg", { viewBox: `0 0 ${W} ${H}`, role: "img", "aria-label": card.querySelector("h2").textContent });
      host.append(svg);
      const n = TL.length;
      const xmax = Math.max(1, n - 1);
      let ymax = 0;
      for (const s of series) for (const p of TL) ymax = Math.max(ymax, s.get(p));
      const sc = scale(ymax, opt.max);
      ymax = sc.step * sc.n;
      const x = (i) => m.l + (i / xmax) * iw, y = (v) => m.t + ih - (v / ymax) * ih;
      for (let k = 0; k <= sc.n; k++) {
        const v = sc.step * k, yy = y(v);
        svg.append(sv("line", { x1: m.l, x2: m.l + iw, y1: yy, y2: yy, stroke: k === 0 ? "var(--axis)" : "var(--grid)", "stroke-width": 1 }));
        const t = sv("text", { x: m.l - 6, y: yy + 4, "text-anchor": "end" }); t.textContent = opt.tick(v); svg.append(t);
      }
      const ticks = Math.min(6, n);
      for (let k = 0; k < ticks; k++) {
        const i = Math.round((k / Math.max(1, ticks - 1)) * xmax);
        const t = sv("text", { x: x(i), y: H - 6, "text-anchor": k === 0 ? "start" : k === ticks - 1 ? "end" : "middle" });
        t.textContent = fmtT(TL[i] ? TL[i].t : 0); svg.append(t);
      }
      const ends = [];
      for (const s of series) {
        const pts = TL.map((p, i) => `${x(i).toFixed(1)},${y(s.get(p)).toFixed(1)}`);
        if (series.length === 1 && n > 1) {
          svg.append(sv("path", { d: `M${x(0)},${y(0)}L${pts.join("L")}L${x(n - 1)},${y(0)}Z`, fill: `var(${s.color})`, "fill-opacity": 0.1 }));
        }
        svg.append(sv("path", { d: "M" + pts.join("L"), fill: "none", stroke: `var(${s.color})`, "stroke-width": 2, "stroke-linejoin": "round", "stroke-linecap": "round" }));
        if (n) ends.push({ s, y: y(s.get(TL[n - 1])) });
      }
      if (series.length > 1) {
        ends.sort((a, b) => a.y - b.y);
        let last = -1e9;
        for (const e of ends) {
          if (e.y - last < 12) continue; // converging lines: the legend and tooltip carry identity
          const t = sv("text", { x: m.l + iw + 6, y: e.y + 4, class: "end-label" }); t.textContent = e.s.short || e.s.name; svg.append(t);
          last = e.y;
        }
      }
      const cross = sv("line", { y1: m.t, y2: m.t + ih, stroke: "var(--axis)", "stroke-width": 1, visibility: "hidden" });
      svg.append(cross);
      const dots = series.map((s) => { const d = sv("circle", { r: 4, fill: `var(${s.color})`, stroke: "var(--surface)", "stroke-width": 2, visibility: "hidden" }); svg.append(d); return d; });
      const hit = sv("rect", { x: m.l, y: m.t, width: iw, height: ih, fill: "transparent", tabindex: 0 });
      svg.append(hit);
      const show = (i) => {
        const p = TL[i]; if (!p) return;
        cross.setAttribute("x1", x(i)); cross.setAttribute("x2", x(i)); cross.setAttribute("visibility", "visible");
        series.forEach((s, k) => { dots[k].setAttribute("cx", x(i)); dots[k].setAttribute("cy", y(s.get(p))); dots[k].setAttribute("visibility", "visible"); });
        tip.textContent = "";
        tip.append(el("div", { class: "t" }, `${fmtT(p.t)} · ${p.vus} VUs`));
        for (const s of series) {
          const row = el("div", { class: "row" });
          const name = el("span", {}, s.name); name.style.setProperty("--c", `var(${s.color})`);
          row.append(name, el("b", {}, opt.value(s.get(p))));
          tip.append(row);
        }
        tip.style.display = "block";
        const box = svg.getBoundingClientRect(), cb = card.getBoundingClientRect();
        const px = box.left - cb.left + (x(i) / W) * box.width;
        const left = px + 12 + tip.offsetWidth > cb.width ? px - 12 - tip.offsetWidth : px + 12;
        tip.style.left = Math.max(4, left) + "px";
        tip.style.top = box.top - cb.top + 8 + "px";
      };
      const hide = () => { tip.style.display = "none"; cross.setAttribute("visibility", "hidden"); dots.forEach((d) => d.setAttribute("visibility", "hidden")); };
      let cur = n - 1;
      hit.addEventListener("pointermove", (ev) => {
        const b = svg.getBoundingClientRect();
        const sx = ((ev.clientX - b.left) / b.width) * W;
        cur = Math.max(0, Math.min(n - 1, Math.round(((sx - m.l) / iw) * xmax))); show(cur);
      });
      hit.addEventListener("pointerleave", hide);
      hit.addEventListener("focus", () => show(cur));
      hit.addEventListener("blur", hide);
      hit.addEventListener("keydown", (ev) => {
        if (ev.key === "ArrowLeft") { cur = Math.max(0, cur - 1); show(cur); ev.preventDefault(); }
        if (ev.key === "ArrowRight") { cur = Math.min(n - 1, cur + 1); show(cur); ev.preventDefault(); }
      });
    };
    draw();
    let raf; new ResizeObserver(() => { cancelAnimationFrame(raf); raf = requestAnimationFrame(draw); }).observe(host);
  }
  const num = (v) => fmtC(v);
  chart($("c-vus"), [{ name: "VUs", color: "--s1", get: (p) => p.vus }], { tick: num, value: (v) => fmtN(v) });
  chart($("c-rps"), [{ name: "req/s", color: "--s1", get: (p) => p.rps }], { tick: num, value: (v) => fmtN(Math.round(v)) + " req/s" });
  chart($("c-lat"), [
    { name: "Average", short: "avg", color: "--s1", get: (p) => p.avg_ms },
    { name: "p90", color: "--s2", get: (p) => p.p90_ms },
    { name: "p95", color: "--s3", get: (p) => p.p95_ms },
    { name: "p99", color: "--s4", get: (p) => p.p99_ms },
  ], { tick: num, value: fmtMs });
  chart($("c-err"), [{ name: "error rate", color: "--s1", get: (p) => p.error_rate }], { tick: (v) => fmtC(v) + "%", value: (v) => v.toFixed(2) + "%", max: 100 });

  // ------------------------------------------------------------ tables
  function table(t, head, rows) {
    const tr = el("tr"); head.forEach((h) => tr.append(el("th", { scope: "col" }, h)));
    const thead = el("thead"); thead.append(tr); t.append(thead);
    const tb = el("tbody");
    for (const r of rows) {
      const row = el("tr");
      for (const c of r) {
        const td = el("td");
        if (c && typeof c === "object" && !(c instanceof Node)) { td.textContent = c.text; if (c.bad) td.className = "bad"; if (c.node) { td.textContent = ""; td.append(...c.node); } }
        else if (c instanceof Node) td.append(c);
        else td.textContent = c;
        row.append(td);
      }
      tb.append(row);
    }
    t.append(tb);
  }
  table($("t-req"), ["Request", "Count", "req/s", "Avg", "p90", "p95", "p99", "Max", "Errors", "HTTP 4xx/5xx", "Failed tests"],
    R.requests.map((q) => [
      { node: [el("span", { class: "method" }, q.method), document.createTextNode(q.path)] },
      fmtN(q.requests), q.rps.toFixed(1), fmtMs(q.avg_ms), fmtMs(q.p90_ms), fmtMs(q.p95_ms), fmtMs(q.p99_ms), fmtMs(q.max_ms),
      { text: q.error_rate.toFixed(2) + "%", bad: q.error_rate > 0 },
      { text: q.http_error_rate.toFixed(2) + "%", bad: q.http_error_rate > 0 },
      { text: q.assertion_failure_rate.toFixed(2) + "%", bad: q.assertion_failure_rate > 0 },
    ]));
  if (R.thresholds.length) {
    $("s-thr").hidden = false;
    table($("t-thr"), ["Threshold", "Actual", "Result"], R.thresholds.map((t) => [t.expr, t.unit === "ms" ? fmtMs(t.actual) : t.unit === "%" ? t.actual.toFixed(2) + "%" : t.unit ? t.actual.toFixed(1) + " " + t.unit : fmtN(t.actual),
      el("span", { class: t.passed ? "mark-ok" : "mark-bad" }, t.passed ? "✓ pass" : "✗ fail")]));
  }
  if (R.assertions.length) {
    $("s-tests").hidden = false;
    table($("t-tests"), ["Test", "Passed", "Failed", "Failure rate"], R.assertions.map((a) => {
      const total = a.passed + a.failed;
      return [`${a.request} › ${a.name}`, fmtN(a.passed), { text: fmtN(a.failed), bad: a.failed > 0 }, (total ? (a.failed * 100 / total).toFixed(2) : "0.00") + "%"];
    }));
  }
  const errRows = [...R.errors.map(([n, c]) => [n, fmtN(c)]), ...R.script_errors.map(([n, c]) => [n, fmtN(c)]), ...R.status_codes.map(([s, c]) => ["HTTP " + s, fmtN(c)])];
  if (errRows.length) { $("s-err").hidden = false; table($("t-err"), ["Outcome", "Count"], errRows); }
  if (R.warnings.length) { $("s-warn").hidden = false; for (const w of R.warnings) $("warnings").append(el("p", { class: "warn" }, w)); }
  table($("t-time"), ["Second", "VUs", "Requests", "Avg", "p90", "p95", "p99", "Max", "Error rate"],
    TL.map((p) => [fmtT(p.t), fmtN(p.vus), fmtN(p.requests), fmtMs(p.avg_ms), fmtMs(p.p90_ms), fmtMs(p.p95_ms), fmtMs(p.p99_ms), fmtMs(p.max_ms), p.error_rate.toFixed(2) + "%"]));
  $("footer").textContent = `Generated by ${R.tool} ${R.version}. Profile ${R.profile.kind}, ${R.profile.vu_hours.toFixed(2)} VU-hours. Percentiles: HDR histograms, ±0.1% (totals) / ±1% (per second).`;
})();
</script>
</body>
</html>
"##;

pub fn render(report: &Report) -> String {
    let json = serde_json::to_string(report).unwrap_or_else(|_| "{}".into());
    // Keep the JSON inert inside <script>: `<` only occurs inside JSON strings,
    // where the < escape is equivalent.
    let json = json.replace('<', "\\u003c");
    TEMPLATE.replace("__DATA__", &json)
}
