// loadman script sandbox: a compact, independent implementation of the
// commonly used parts of the Postman scripting API (pm.*, legacy globals and
// a chai-style expect). It runs inside QuickJS, one engine per worker thread,
// with per-virtual-user variable state kept in VUS[vu].
"use strict";

const TOMB = Symbol("unset");
const SCRIPTS = Object.create(null);
const VUS = Object.create(null);
let BASE = { e: {}, c: {}, g: {}, envName: undefined };
const PARAMS = ["pm", "postman", "tests", "responseBody", "responseCode", "responseTime",
  "responseHeaders", "request", "iteration", "data", "environment", "globals", "console",
  "require", "setTimeout", "clearTimeout"];

globalThis.__lm_setBase = (json) => { BASE = JSON.parse(json); };
globalThis.__lm_initVu = (vu, dataJson) => {
  VUS[vu] = { e: {}, c: {}, g: {}, l: {}, eCleared: false, cCleared: false, gCleared: false, d: JSON.parse(dataJson || "null") || {} };
};
globalThis.__lm_setData = (vu, dataJson) => {
  const s = VUS[vu]; if (!s) return;
  s.d = JSON.parse(dataJson || "null") || {}; s.l = {};
};
globalThis.__lm_dropVu = (vu) => { delete VUS[vu]; };
// Scripts compile to async functions so they can `await` (pm.sendRequest
// without a callback returns a promise).
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
globalThis.__lm_compile = (id, src) => {
  try { SCRIPTS[id] = new AsyncFunction(...PARAMS, src); return ""; }
  catch (e) { SCRIPTS[id] = e; return errText(e); }
};

function errText(e) {
  if (e && typeof e === "object") return (e.name || "Error") + ": " + (e.message || String(e));
  return "Error: " + String(e);
}
function toStr(v) {
  if (typeof v === "string") return v;
  if (v === undefined || v === null) return "";
  if (typeof v === "object") { try { return JSON.stringify(v); } catch (_) { return String(v); } }
  return String(v);
}
const has = (o, k) => Object.prototype.hasOwnProperty.call(o, k);

// ---------------------------------------------------------------- variables
function makeScope(state, code, ops) {
  const flag = code + "Cleared";
  const scope = {
    get(k) {
      const o = state[code];
      if (has(o, k)) { const v = o[k]; return v === TOMB ? undefined : v; }
      if (state[flag]) return undefined;
      return BASE[code][k];
    },
    set(k, v) { k = String(k); state[code][k] = v; ops.push(["s", code, k, toStr(v)]); },
    unset(k) { k = String(k); state[code][k] = TOMB; ops.push(["u", code, k]); },
    has(k) { return scope.get(k) !== undefined; },
    clear() { state[code] = {}; state[flag] = true; ops.push(["c", code]); },
    toObject() {
      const out = state[flag] ? {} : Object.assign({}, BASE[code]);
      for (const k of Object.keys(state[code])) { const v = state[code][k]; if (v === TOMB) delete out[k]; else out[k] = v; }
      return out;
    },
    replaceIn(str) { return replaceIn(str, (k) => scope.get(k)); },
  };
  return scope;
}

function replaceIn(str, get) {
  return String(str).replace(/\{\{([^{}]+?)\}\}/g, (m, name) => {
    const v = get(name);
    return v === undefined ? m : toStr(v);
  });
}

// ---------------------------------------------------------------- headers
function HeaderList(pairs, hdrOps) {
  this._p = pairs.map(([k, v]) => ({ key: k, value: v }));
  this._ops = hdrOps;
}
HeaderList.prototype = {
  _idx(name) { const n = String(name).toLowerCase(); return this._p.findIndex((h) => h.key.toLowerCase() === n); },
  get(name) { const i = this._idx(name); return i < 0 ? undefined : this._p[i].value; },
  has(name, value) { const i = this._idx(name); return i >= 0 && (value === undefined || this._p[i].value === String(value)); },
  one(name) { const i = this._idx(name); return i < 0 ? undefined : this._p[i]; },
  all() { return this._p.slice(); },
  count() { return this._p.length; },
  each(fn) { this._p.forEach(fn); },
  map(fn) { return this._p.map(fn); },
  filter(fn) { return this._p.filter(fn); },
  find(fn) { return this._p.find(fn); },
  toObject() { const o = {}; for (const h of this._p) o[h.key] = h.value; return o; },
  toString() { return this._p.map((h) => h.key + ": " + h.value).join("\n"); },
  _kv(h, v) {
    if (typeof h === "string" && v !== undefined) return { key: h, value: toStr(v) };
    if (typeof h === "string") { const i = h.indexOf(":"); return { key: h.slice(0, i).trim(), value: h.slice(i + 1).trim() }; }
    return { key: String(h.key), value: toStr(h.value) };
  },
  add(h, v) { if (!this._ops) throw new Error("response headers are read-only"); const kv = this._kv(h, v); this._p.push(kv); this._ops.push(["add", kv.key, kv.value]); },
  upsert(h, v) {
    if (!this._ops) throw new Error("response headers are read-only");
    const kv = this._kv(h, v); const i = this._idx(kv.key);
    if (i >= 0) this._p[i].value = kv.value; else this._p.push(kv);
    this._ops.push(["set", kv.key, kv.value]);
  },
  remove(name) {
    if (!this._ops) throw new Error("response headers are read-only");
    const n = String(typeof name === "object" ? name.key : name);
    this._p = this._p.filter((h) => h.key.toLowerCase() !== n.toLowerCase());
    this._ops.push(["rm", n]);
  },
};

// ---------------------------------------------------------------- assertions
class AssertionError extends Error { constructor(m) { super(m); this.name = "AssertionError"; } }

function fmt(v) {
  if (typeof v === "string") return JSON.stringify(v.length > 120 ? v.slice(0, 120) + "…" : v);
  if (v === undefined) return "undefined";
  if (typeof v === "function") return "[Function]";
  if (v && v.__lmResponse) return "response " + v.code;
  try { const s = JSON.stringify(v); return s === undefined ? String(v) : s.length > 160 ? s.slice(0, 160) + "…" : s; }
  catch (_) { return String(v); }
}
function typeOf(v) {
  if (v === null) return "null";
  if (Array.isArray(v)) return "array";
  if (v instanceof RegExp) return "regexp";
  if (v instanceof Date) return "date";
  return typeof v;
}
function deepEq(a, b) {
  if (a === b) return true;
  if (typeof a === "number" && typeof b === "number") return Number.isNaN(a) && Number.isNaN(b);
  if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  if (a instanceof Date && b instanceof Date) return a.getTime() === b.getTime();
  const ka = Object.keys(a), kb = Object.keys(b);
  if (ka.length !== kb.length) return false;
  return ka.every((k) => has(b, k) && deepEq(a[k], b[k]));
}
function subset(sub, obj) {
  if (typeof sub !== "object" || sub === null) return deepEq(sub, obj);
  if (typeof obj !== "object" || obj === null) return false;
  return Object.keys(sub).every((k) => has(obj, k) && deepEq(sub[k], obj[k]));
}
function getPath(obj, path) {
  const parts = String(path).replace(/\[(\w+)\]/g, ".$1").split(".").filter(Boolean);
  let cur = obj;
  for (const p of parts) { if (cur === null || cur === undefined || !(p in Object(cur))) return { found: false }; cur = cur[p]; }
  return { found: true, value: cur };
}

function Assertion(obj, msg) { this._obj = obj; this._msg = msg; this._not = false; this._deep = false; this._any = false; }
const A = Assertion.prototype;
A._assert = function (ok, msg, negMsg) {
  if (this._not ? ok : !ok) throw new AssertionError((this._msg ? this._msg + ": " : "") + (this._not ? negMsg : msg));
  return this;
};
A._resp = function () {
  if (!this._obj || !this._obj.__lmResponse) throw new AssertionError("expected a response object but got " + fmt(this._obj));
  return this._obj;
};
for (const n of ["to", "be", "been", "is", "that", "which", "and", "has", "have", "with", "at", "of", "same", "but", "does", "still", "also", "nested", "own"]) {
  Object.defineProperty(A, n, { get() { return this; } });
}
const prop = (n, fn) => Object.defineProperty(A, n, { get() { fn.call(this); return this; } });
prop("not", function () { this._not = !this._not; });
prop("deep", function () { this._deep = true; });
prop("any", function () { this._any = true; });
prop("all", function () { this._any = false; });
prop("true", function () { this._assert(this._obj === true, `expected ${fmt(this._obj)} to be true`, "expected true to be false"); });
prop("false", function () { this._assert(this._obj === false, `expected ${fmt(this._obj)} to be false`, "expected false to be true"); });
prop("null", function () { this._assert(this._obj === null, `expected ${fmt(this._obj)} to be null`, "expected value not to be null"); });
prop("undefined", function () { this._assert(this._obj === undefined, `expected ${fmt(this._obj)} to be undefined`, "expected value not to be undefined"); });
prop("NaN", function () { this._assert(Number.isNaN(this._obj), `expected ${fmt(this._obj)} to be NaN`, "expected value not to be NaN"); });
prop("exist", function () { this._assert(this._obj !== null && this._obj !== undefined, `expected ${fmt(this._obj)} to exist`, `expected ${fmt(this._obj)} to not exist`); });
prop("empty", function () {
  const o = this._obj;
  const empty = typeof o === "string" || Array.isArray(o) ? o.length === 0 : o && typeof o === "object" ? Object.keys(o).length === 0 : false;
  this._assert(empty, `expected ${fmt(o)} to be empty`, `expected ${fmt(o)} not to be empty`);
});
prop("ok", function () {
  const o = this._obj;
  if (o && o.__lmResponse) this._assert(o.code === 200, `expected response to have status code 200 but got ${o.code}`, "expected response to not have status code 200");
  else this._assert(!!o, `expected ${fmt(o)} to be truthy`, `expected ${fmt(o)} to be falsy`);
});
const classes = { info: 1, success: 2, redirection: 3, clientError: 4, serverError: 5 };
for (const [n, c] of Object.entries(classes)) {
  prop(n, function () {
    const r = this._resp();
    this._assert(Math.floor(r.code / 100) === c, `expected response code to be ${c}XX but found ${r.code}`, `expected response code to not be ${c}XX`);
  });
}
prop("error", function () {
  const r = this._resp(); const c = Math.floor(r.code / 100);
  this._assert(c === 4 || c === 5, `expected response code to be 4XX or 5XX but found ${r.code}`, "expected response code to not be 4XX or 5XX");
});
const codes = { accepted: 202, withoutContent: 204, badRequest: 400, unauthorized: 401, unauthorised: 401, forbidden: 403, notFound: 404, notAcceptable: 406, rateLimited: 429 };
for (const [n, c] of Object.entries(codes)) {
  prop(n, function () { const r = this._resp(); this._assert(r.code === c, `expected response to have status code ${c} but got ${r.code}`, `expected response to not have status code ${c}`); });
}
prop("json", function () {
  const r = this._resp(); let ok = true; try { r.json(); } catch (_) { ok = false; }
  this._assert(ok, "expected response body to be a valid json", "expected response body not to be a valid json");
});
prop("withBody", function () { const r = this._resp(); this._assert(r.text().length > 0, "expected response to have content in body", "expected response to not have content in body"); });

function method(names, fn) { for (const n of names) A[n] = function (...args) { fn.apply(this, args); return this; }; }
method(["equal", "equals", "eq"], function (v) {
  const ok = this._deep ? deepEq(this._obj, v) : this._obj === v;
  this._assert(ok, `expected ${fmt(this._obj)} to ${this._deep ? "deeply " : ""}equal ${fmt(v)}`, `expected ${fmt(this._obj)} to not equal ${fmt(v)}`);
});
method(["eql", "eqls"], function (v) { this._assert(deepEq(this._obj, v), `expected ${fmt(this._obj)} to deeply equal ${fmt(v)}`, `expected ${fmt(this._obj)} to not deeply equal ${fmt(v)}`); });
method(["above", "gt", "greaterThan"], function (n) { this._assert(this._obj > n, `expected ${fmt(this._obj)} to be above ${n}`, `expected ${fmt(this._obj)} to be at most ${n}`); });
method(["below", "lt", "lessThan"], function (n) { this._assert(this._obj < n, `expected ${fmt(this._obj)} to be below ${n}`, `expected ${fmt(this._obj)} to be at least ${n}`); });
method(["least", "gte", "greaterThanOrEqual"], function (n) { this._assert(this._obj >= n, `expected ${fmt(this._obj)} to be at least ${n}`, `expected ${fmt(this._obj)} to be below ${n}`); });
method(["most", "lte", "lessThanOrEqual"], function (n) { this._assert(this._obj <= n, `expected ${fmt(this._obj)} to be at most ${n}`, `expected ${fmt(this._obj)} to be above ${n}`); });
method(["within"], function (a, b) { this._assert(this._obj >= a && this._obj <= b, `expected ${fmt(this._obj)} to be within ${a}..${b}`, `expected ${fmt(this._obj)} to not be within ${a}..${b}`); });
method(["closeTo", "approximately"], function (n, d) { this._assert(Math.abs(this._obj - n) <= d, `expected ${fmt(this._obj)} to be close to ${n} +/- ${d}`, `expected ${fmt(this._obj)} not to be close to ${n} +/- ${d}`); });
method(["a", "an"], function (t) { const actual = typeOf(this._obj); this._assert(actual === String(t).toLowerCase(), `expected ${fmt(this._obj)} to be a${/^[aeiou]/.test(t) ? "n" : ""} ${t}`, `expected ${fmt(this._obj)} not to be a ${t}`); });
method(["include", "includes", "contain", "contains"], function (v) {
  const o = this._obj; let ok;
  if (typeof o === "string") ok = o.indexOf(v) >= 0;
  else if (Array.isArray(o)) ok = o.some((x) => (this._deep ? deepEq(x, v) : x === v || deepEq(x, v)));
  else if (o && typeof o === "object") ok = subset(v, o);
  else ok = false;
  this._assert(ok, `expected ${fmt(o)} to include ${fmt(v)}`, `expected ${fmt(o)} to not include ${fmt(v)}`);
});
method(["string"], function (s) { this._assert(typeof this._obj === "string" && this._obj.indexOf(s) >= 0, `expected ${fmt(this._obj)} to contain ${fmt(s)}`, `expected ${fmt(this._obj)} to not contain ${fmt(s)}`); });
method(["match", "matches"], function (re) { this._assert(re.test(String(this._obj)), `expected ${fmt(this._obj)} to match ${re}`, `expected ${fmt(this._obj)} not to match ${re}`); });
method(["oneOf"], function (list) { this._assert(list.some((x) => deepEq(x, this._obj)), `expected ${fmt(this._obj)} to be one of ${fmt(list)}`, `expected ${fmt(this._obj)} to not be one of ${fmt(list)}`); });
method(["instanceOf", "instanceof"], function (C) { this._assert(this._obj instanceof C, `expected ${fmt(this._obj)} to be an instance of ${C && C.name}`, `expected value not to be an instance of ${C && C.name}`); });
method(["satisfy", "satisfies"], function (fn) { this._assert(!!fn(this._obj), `expected ${fmt(this._obj)} to satisfy the predicate`, `expected ${fmt(this._obj)} to not satisfy the predicate`); });
method(["lengthOf", "length"], function (n) {
  const len = this._obj == null ? undefined : this._obj.length !== undefined ? this._obj.length : this._obj.size;
  this._assert(len === n, `expected ${fmt(this._obj)} to have a length of ${n} but got ${len}`, `expected ${fmt(this._obj)} to not have a length of ${n}`);
});
method(["members", "sameMembers"], function (list) {
  const o = this._obj;
  const ok = Array.isArray(o) && o.length === list.length && list.every((x) => o.some((y) => deepEq(x, y)));
  this._assert(ok, `expected ${fmt(o)} to have the same members as ${fmt(list)}`, `expected ${fmt(o)} to not have the same members as ${fmt(list)}`);
});
method(["keys", "key"], function (...ks) {
  if (ks.length === 1 && Array.isArray(ks[0])) ks = ks[0];
  else if (ks.length === 1 && ks[0] && typeof ks[0] === "object") ks = Object.keys(ks[0]);
  const actual = Object.keys(Object(this._obj));
  const ok = this._any ? ks.some((k) => actual.includes(k)) : ks.every((k) => actual.includes(k)) && actual.length === ks.length;
  this._assert(ok, `expected ${fmt(this._obj)} to have ${this._any ? "any" : "all"} keys ${fmt(ks)}`, `expected ${fmt(this._obj)} to not have keys ${fmt(ks)}`);
});
A.property = function (name, ...val) {
  const o = this._obj;
  const found = o !== null && o !== undefined && name in Object(o);
  if (val.length === 0) {
    this._assert(found, `expected ${fmt(o)} to have property ${fmt(name)}`, `expected ${fmt(o)} to not have property ${fmt(name)}`);
  } else {
    const ok = found && (this._deep ? deepEq(o[name], val[0]) : o[name] === val[0]);
    this._assert(ok, `expected ${fmt(o)} to have property ${fmt(name)} of ${fmt(val[0])}${found ? ", but got " + fmt(o[name]) : ""}`, `expected ${fmt(o)} to not have property ${fmt(name)} of ${fmt(val[0])}`);
  }
  if (found && !this._not) this._obj = o[name];
  return this;
};
A.ownProperty = A.haveOwnProperty = A.property;
A.status = function (s) {
  const r = this._resp();
  if (typeof s === "string" && !/^\d+$/.test(s)) {
    this._assert(String(r.status).toUpperCase() === s.toUpperCase(), `expected response to have status reason ${fmt(s)} but got ${fmt(r.status)}`, `expected response to not have status reason ${fmt(s)}`);
  } else {
    this._assert(r.code === Number(s), `expected response to have status code ${s} but got ${r.code}`, `expected response to not have status code ${s}`);
  }
  return this;
};
A.statusCode = A.status;
A.statusCodeClass = function (c) { const r = this._resp(); return this._assert(Math.floor(r.code / 100) === c, `expected response code to be ${c}XX but found ${r.code}`, `expected response code to not be ${c}XX`); };
A.header = function (name, value) {
  const r = this._resp(); const v = r.headers.get(name);
  if (value === undefined) return this._assert(v !== undefined, `expected response to have header ${fmt(name)}`, `expected response to not have header ${fmt(name)}`);
  return this._assert(v === String(value), `expected response to have header ${fmt(name)} with value ${fmt(value)} but got ${fmt(v)}`, `expected response to not have header ${fmt(name)} with value ${fmt(value)}`);
};
A.body = function (expected) {
  const r = this._resp();
  if (expected === undefined) return this._assert(r.text().length > 0, "expected response to have content in body", "expected response to not have content in body");
  if (typeof expected === "string") return this._assert(r.text() === expected, `expected response body to equal ${fmt(expected)}`, `expected response body to not equal ${fmt(expected)}`);
  let json; try { json = r.json(); } catch (_) { json = undefined; }
  return this._assert(deepEq(json, expected), `expected response body json to equal ${fmt(expected)}`, `expected response body json to not equal ${fmt(expected)}`);
};
A.jsonBody = function (path, ...val) {
  const r = this._resp(); let json, ok = true;
  try { json = r.json(); } catch (_) { ok = false; }
  if (!ok) return this._assert(false, "expected response body to be a valid json", "expected response body not to be a valid json");
  if (path === undefined) return this._assert(true, "", "expected response body not to be a valid json");
  if (typeof path === "object") return this._assert(deepEq(json, path), `expected response body json to equal ${fmt(path)}`, `expected response body json to not equal ${fmt(path)}`);
  const got = getPath(json, path);
  if (val.length === 0) return this._assert(got.found, `expected response body json to have path ${fmt(path)}`, `expected response body json to not have path ${fmt(path)}`);
  return this._assert(got.found && deepEq(got.value, val[0]), `expected response body json at ${fmt(path)} to contain ${fmt(val[0])} but got ${fmt(got.value)}`, `expected response body json at ${fmt(path)} to not contain ${fmt(val[0])}`);
};
A.jsonSchema = function () { throw new Error("jsonSchema assertions are not supported by loadman yet"); };
A.responseTime = function () { throw new Error("use pm.expect(pm.response.responseTime).to.be.below(ms)"); };

function expect(v, msg) { return new Assertion(v, msg); }
expect.fail = (m) => { throw new AssertionError(m || "expect.fail()"); };

// ---------------------------------------------------------------- base64
const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
globalThis.btoa = (s) => {
  s = String(s); let out = "";
  for (let i = 0; i < s.length; i += 3) {
    const a = s.charCodeAt(i), b = s.charCodeAt(i + 1), c = s.charCodeAt(i + 2);
    const n = (a << 16) | ((b || 0) << 8) | (c || 0);
    out += B64[(n >> 18) & 63] + B64[(n >> 12) & 63] + (i + 1 < s.length ? B64[(n >> 6) & 63] : "=") + (i + 2 < s.length ? B64[n & 63] : "=");
  }
  return out;
};
globalThis.atob = (s) => {
  s = String(s).replace(/[^A-Za-z0-9+/]/g, ""); let out = "";
  for (let i = 0; i < s.length; i += 4) {
    const n = (B64.indexOf(s[i]) << 18) | (B64.indexOf(s[i + 1]) << 12) | ((B64.indexOf(s[i + 2]) & 63) << 6) | (B64.indexOf(s[i + 3]) & 63);
    out += String.fromCharCode((n >> 16) & 255);
    if (s[i + 2] !== undefined) out += String.fromCharCode((n >> 8) & 255);
    if (s[i + 3] !== undefined) out += String.fromCharCode(n & 255);
  }
  return out;
};

// ---------------------------------------------------------------- modules
// crypto-js uses crypto.getRandomValues for salts and IVs. Not cryptographically
// secure here, which is acceptable for generating test traffic.
if (!globalThis.crypto) {
  globalThis.crypto = {
    getRandomValues(a) {
      const max = 2 ** (8 * (a.BYTES_PER_ELEMENT || 1));
      for (let i = 0; i < a.length; i++) a[i] = Math.floor(Math.random() * max);
      return a;
    },
  };
}
function uuidV4() {
  const b = crypto.getRandomValues(new Uint8Array(16));
  b[6] = (b[6] & 0x0f) | 0x40; b[8] = (b[8] & 0x3f) | 0x80;
  const h = Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}
const MODULES = Object.create(null);
const LOADING = new Set();
const AVAILABLE = "crypto-js, lodash, moment, uuid, chai, atob, btoa";
// Bundled libraries are evaluated on first use and cached per engine; their
// sources are set by the host as __LM_SOURCES.
function requireModule(name) {
  const key = String(name).replace(/^node:/, "");
  if (key in MODULES) return MODULES[key];
  // lodash reads `root._` while it loads; the `_` getter must not re-enter.
  if (LOADING.has(key)) return undefined;
  let exp;
  switch (key) {
    case "uuid": exp = { v4: uuidV4 }; break;
    case "chai": exp = { expect, AssertionError }; break;
    case "atob": exp = globalThis.atob; break;
    case "btoa": exp = globalThis.btoa; break;
    default: {
      const src = globalThis.__LM_SOURCES && globalThis.__LM_SOURCES[key];
      if (typeof src !== "string") throw new Error(`module '${key}' is not available in loadman (available: ${AVAILABLE})`);
      const module = { exports: {} };
      LOADING.add(key);
      try {
        new Function("module", "exports", "define", src).call(globalThis, module, module.exports, undefined);
      } finally {
        LOADING.delete(key);
      }
      exp = module.exports;
    }
  }
  MODULES[key] = exp;
  return exp;
}
// Postman scripts can use lodash as `_` without requiring it.
Object.defineProperty(globalThis, "_", { get: () => requireModule("lodash"), configurable: true });

function makeResponse(res, body) {
  let parsed, parsedOk = false;
  const response = {
    __lmResponse: true, code: res.code, status: res.status, responseTime: res.time, responseSize: res.size,
    headers: new HeaderList(res.headers || [], null),
    text: () => body,
    json() { if (!parsedOk) { parsed = JSON.parse(body); parsedOk = true; } return parsed; },
    reason: () => res.status,
    details: () => ({ code: res.code, name: res.status }),
  };
  Object.defineProperty(response, "to", { get() { return new Assertion(response); } });
  return response;
}

// The request description pm.sendRequest accepts: a URL string, or an object
// with url, method, header (list, object or HeaderList) and body.
function normalizeRequest(r) {
  if (typeof r === "string") return { url: r, method: "GET", headers: [], body: null };
  if (!r || typeof r !== "object") throw new TypeError("pm.sendRequest needs a URL or a request object");
  const url = typeof r.url === "string" ? r.url : r.url && (r.url.raw || String(r.url));
  if (!url) throw new TypeError("pm.sendRequest: the request has no url");
  const method = String(r.method || "GET").toUpperCase();
  let headers = [];
  const h = r.header || r.headers;
  if (h instanceof HeaderList) headers = h.all().map((x) => [x.key, x.value]);
  else if (Array.isArray(h)) {
    headers = h.filter((x) => x && !x.disabled).map((x) => {
      if (typeof x !== "string") return [String(x.key), toStr(x.value)];
      const i = x.indexOf(":");
      return [x.slice(0, i).trim(), x.slice(i + 1).trim()];
    });
  } else if (h && typeof h === "object") headers = Object.entries(h).map(([k, v]) => [k, toStr(v)]);
  const hasType = () => headers.some(([k]) => k.toLowerCase() === "content-type");
  let body = null;
  const b = r.body;
  if (typeof b === "string") body = b;
  else if (b && typeof b === "object") {
    if (b.mode === "urlencoded") {
      body = (b.urlencoded || []).filter((x) => !x.disabled)
        .map((x) => encodeURIComponent(x.key) + "=" + encodeURIComponent(toStr(x.value))).join("&");
      if (!hasType()) headers.push(["Content-Type", "application/x-www-form-urlencoded"]);
    } else if (b.mode === "graphql") {
      const g = b.graphql || {};
      body = JSON.stringify({ query: g.query, variables: typeof g.variables === "string" ? JSON.parse(g.variables || "{}") : g.variables || {} });
      if (!hasType()) headers.push(["Content-Type", "application/json"]);
    } else if (b.mode === "formdata" || b.mode === "file") {
      throw new Error(`pm.sendRequest: ${b.mode} bodies are not supported yet`);
    } else if (b.raw !== undefined) {
      body = toStr(b.raw);
      const lang = b.options && b.options.raw && b.options.raw.language;
      if (lang === "json" && !hasType()) headers.push(["Content-Type", "application/json"]);
    }
  }
  return { url, method, headers, body };
}

// The script currently running, so the host can collect partial results when
// a promise never settles.
let CURRENT = null;
globalThis.__lm_partial = () => JSON.stringify({
  ops: CURRENT ? CURRENT.ops : [], headers: CURRENT ? CURRENT.hdrOps : [], tests: CURRENT ? CURRENT.results : [],
  logs: CURRENT ? CURRENT.logs : [], cookies: CURRENT ? CURRENT.cookieOps : [], next: CURRENT ? CURRENT.next : undefined,
  error: (CURRENT && CURRENT.error) || "Error: a promise or async test never finished (pm.test with done() that is never called?)",
});

// ---------------------------------------------------------------- exec
globalThis.__LM_VERBOSE = false;

globalThis.__lm_exec = async (vu, idsJson, phase, ctxJson, body) => {
  const state = VUS[vu] || (globalThis.__lm_initVu(vu, "null"), VUS[vu]);
  const ctx = JSON.parse(ctxJson);
  const ops = [], hdrOps = [], results = [], logs = [], cookieOps = [], pending = [];
  const run = (CURRENT = { ops, hdrOps, results, logs, cookieOps, error: null, next: undefined });
  // The last call wins; null ends the iteration.
  const setNextRequest = (to) => { run.next = { to: to === null || to === undefined ? null : String(to) }; };

  const env = makeScope(state, "e", ops), coll = makeScope(state, "c", ops), glob = makeScope(state, "g", ops);
  env.name = BASE.envName;
  const locals = {
    get(k) { const v = state.l[k]; return v === TOMB ? undefined : v; },
    set(k, v) { k = String(k); state.l[k] = v; ops.push(["s", "l", k, toStr(v)]); },
    unset(k) { k = String(k); delete state.l[k]; ops.push(["u", "l", k]); },
  };
  const resolve = (k) => {
    if (has(state.l, k)) return locals.get(k);
    if (has(state.d, k)) return state.d[k];
    const e = env.get(k); if (e !== undefined) return e;
    const c = coll.get(k); if (c !== undefined) return c;
    return glob.get(k);
  };
  const variables = {
    get: resolve,
    set: locals.set,
    unset: locals.unset,
    has: (k) => resolve(k) !== undefined,
    toObject() { return Object.assign({}, glob.toObject(), coll.toObject(), env.toObject(), state.d, state.l); },
    replaceIn: (s) => replaceIn(s, resolve),
  };
  const iterationData = {
    get: (k) => state.d[k], has: (k) => has(state.d, k), toObject: () => Object.assign({}, state.d),
    unset() {}, toJSON: () => Object.assign({}, state.d),
  };

  const reqHeaders = new HeaderList(ctx.reqHeaders || [], phase === "pre" ? hdrOps : null);
  const request = {
    url: { toString: () => ctx.url, getHost: () => { const m = /^[a-z]+:\/\/([^/:?#]+)/i.exec(ctx.url); return m ? m[1] : ""; } },
    method: ctx.method, headers: reqHeaders, name: ctx.name, id: String(ctx.id),
    body: { raw: ctx.body || "", mode: "raw", toString: () => ctx.body || "" },
    addHeader: (h) => reqHeaders.add(h), removeHeader: (n) => reqHeaders.remove(n),
  };

  const response = ctx.res ? makeResponse(ctx.res, body) : undefined;

  // Tests may be async (return a promise) or take a `done` callback; they are
  // recorded when they settle.
  const record = (name, ok, msg) => results.push([String(name), ok, msg]);
  const testFn = (name, fn) => {
    if (typeof fn !== "function") { record(name, true, null); return; }
    try {
      const r = fn.length > 0 ? new Promise((resolve, reject) => fn((e) => (e ? reject(e) : resolve()))) : fn();
      if (r && typeof r.then === "function") pending.push(Promise.resolve(r).then(() => record(name, true, null), (e) => record(name, false, errText(e))));
      else record(name, true, null);
    } catch (e) { record(name, false, errText(e)); }
  };
  testFn.skip = () => {};

  const sendRequest = (r, cb) => {
    let err = null, res;
    try {
      const out = JSON.parse(globalThis.__lm_http(JSON.stringify(normalizeRequest(r))));
      if (out.error) err = Object.assign(new Error(out.error), { code: out.error });
      else res = makeResponse(out, out.body);
    } catch (e) { err = e; }
    if (typeof cb === "function") { cb(err, res); return undefined; }
    return err ? Promise.reject(err) : Promise.resolve(res);
  };

  // Cookies the VU's jar holds for this request's URL.
  const cookieList = (ctx.cookies || []).map(([name, value]) => ({ name, value }));
  const cookies = {
    get: (n) => { const c = cookieList.find((c) => c.name === n); return c ? c.value : undefined; },
    has: (n) => cookieList.some((c) => c.name === n),
    toObject: () => Object.fromEntries(cookieList.map((c) => [c.name, c.value])),
    all: () => cookieList.slice(),
    count: () => cookieList.length,
    each: (f) => cookieList.forEach(f),
    jar() {
      return {
        get(_url, name, cb) { if (cb) cb(null, cookies.get(name)); },
        getAll(_url, cb) { if (cb) cb(null, cookieList.slice()); },
        set(url, name, value, cb) {
          if (name && typeof name === "object") { cb = value; value = name.value; name = name.name; }
          cookieOps.push([String(url), String(name), toStr(value)]);
          if (typeof cb === "function") cb(null);
        },
        unset(url, name, cb) { cookieOps.push([String(url), String(name), null]); if (typeof cb === "function") cb(null); },
      };
    },
  };

  const pm = {
    environment: env, collectionVariables: coll, globals: glob, variables, iterationData,
    request, response,
    info: { iteration: ctx.iteration, iterationCount: Infinity, requestName: ctx.name, requestId: ctx.itemId || String(ctx.id), eventName: phase === "pre" ? "prerequest" : "test" },
    test: testFn, expect,
    sendRequest, cookies,
    execution: { skipRequest() { ops.push(["skip"]); }, setNextRequest },
    vault: { get: async () => undefined },
    require: (n) => { throw new Error(`pm.require('${n}'): package libraries are not available in loadman; use require() for ${AVAILABLE}`); },
  };

  const legacyTests = {};
  const postman = {
    setEnvironmentVariable: (k, v) => env.set(k, v), getEnvironmentVariable: (k) => env.get(k), clearEnvironmentVariable: (k) => env.unset(k),
    clearEnvironmentVariables: () => env.clear(),
    setGlobalVariable: (k, v) => glob.set(k, v), getGlobalVariable: (k) => glob.get(k), clearGlobalVariable: (k) => glob.unset(k),
    clearGlobalVariables: () => glob.clear(),
    getResponseHeader: (n) => (response ? response.headers.get(n) : undefined),
    setNextRequest,
  };
  const cons = {};
  for (const lvl of ["log", "info", "warn", "error", "debug"]) {
    cons[lvl] = (...a) => { if (globalThis.__LM_VERBOSE && logs.length < 20) logs.push(lvl + ": " + a.map((x) => (typeof x === "string" ? x : fmt(x))).join(" ")); };
  }
  const legacyHeaders = response ? response.headers.toObject() : {};
  const args = [pm, postman, legacyTests, ctx.res ? body : undefined,
    ctx.res ? { code: ctx.res.code, name: ctx.res.status, detail: ctx.res.status } : undefined,
    ctx.res ? ctx.res.time : undefined, legacyHeaders, { url: ctx.url, method: ctx.method, headers: reqHeaders.toObject(), name: ctx.name },
    ctx.iteration, state.d, env.toObject(), glob.toObject(), cons, requireModule,
    // Timers run as soon as the script yields; there is no real waiting.
    (fn, _ms, ...a) => { if (typeof fn === "function") Promise.resolve().then(() => fn(...a)); return 0; }, () => {}];

  for (const id of JSON.parse(idsJson)) {
    const fn = SCRIPTS[id];
    if (typeof fn !== "function") { run.error = run.error || ("SyntaxError: " + (fn && fn.message ? fn.message : "script did not compile")); continue; }
    try { await fn.apply(undefined, args); }
    catch (e) { run.error = run.error || errText(e); }
  }
  while (pending.length) await Promise.all(pending.splice(0));
  for (const k of Object.keys(legacyTests)) results.push([k, !!legacyTests[k], legacyTests[k] ? null : "AssertionError: tests[\"" + k + "\"] is falsy"]);
  CURRENT = null;
  return JSON.stringify({ ops, headers: hdrOps, tests: results, error: run.error, logs, cookies: cookieOps, next: run.next });
};
