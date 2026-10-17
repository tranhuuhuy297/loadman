// Minimal closed-loop load generator in Node.js (keep-alive http.Agent), used as
// a lower bound for the overhead of Node-based load engines.
import http from "node:http";
const [url, vus = "100", secs = "15"] = process.argv.slice(2);
const agent = new http.Agent({ keepAlive: true, maxSockets: Infinity });
const u = new URL(url);
const lat = [];
let errors = 0;
const end = Date.now() + Number(secs) * 1000;
function once() {
  return new Promise((resolve) => {
    const t0 = process.hrtime.bigint();
    const req = http.request({ host: u.hostname, port: u.port, path: u.pathname, agent }, (res) => {
      res.on("data", () => {});
      res.on("end", () => { lat.push(Number(process.hrtime.bigint() - t0) / 1e6); resolve(); });
    });
    req.on("error", () => { errors++; resolve(); });
    req.end();
  });
}
async function vu() { while (Date.now() < end) await once(); }
const t = Date.now();
await Promise.all(Array.from({ length: Number(vus) }, vu));
const el = (Date.now() - t) / 1000;
lat.sort((a, b) => a - b);
const q = (p) => lat[Math.min(lat.length - 1, Math.floor(p * lat.length))].toFixed(2);
console.log(JSON.stringify({ requests: lat.length, rps: +(lat.length / el).toFixed(0), p50: q(0.5), p95: q(0.95), p99: q(0.99), errors }));
