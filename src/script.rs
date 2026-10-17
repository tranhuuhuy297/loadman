//! Pre-request and test scripts, executed in embedded QuickJS engines.
//!
//! A small pool of OS threads each owns one engine. A virtual user is pinned
//! to one engine (VU index modulo pool size), so its variable state lives in
//! that engine and job order is preserved. Scripts are compiled once per
//! engine at start-up. The HTTP side of the VU keeps its own copy of the
//! variables and applies the set/unset operations each script reports, so no
//! variable state crosses threads on the hot path.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use rquickjs::{Context, Function, Runtime};
use serde::Deserialize;
use tokio::sync::oneshot;

use crate::vars::{BaseVars, Row, Scope, VarOp};

const PRELUDE: &str = include_str!("js/prelude.js");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Pre,
    Test,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HeaderOp {
    Set(String, String),
    Add(String, String),
    Remove(String),
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub ops: Vec<VarOp>,
    pub headers: Vec<HeaderOp>,
    /// (test name, passed)
    pub tests: Vec<(String, bool)>,
    /// First failure message, kept for `--verbose` output.
    pub first_failure: Option<String>,
    pub error: Option<String>,
    pub skip_request: bool,
    pub logs: Vec<String>,
}

pub struct ExecResponse<'a> {
    pub code: u16,
    pub status: &'a str,
    pub time_ms: f64,
    pub size: u64,
    pub headers: Vec<(String, String)>,
}

pub struct ExecContext<'a> {
    pub name: &'a str,
    pub id: usize,
    pub iteration: u64,
    pub method: &'a str,
    pub url: &'a str,
    pub headers: &'a [(String, String)],
    pub body: Option<&'a str>,
    pub response: Option<ExecResponse<'a>>,
}

enum Job {
    Init { vu: usize, data: String },
    SetData { vu: usize, data: String },
    Drop { vu: usize },
    Exec { vu: usize, ids: Arc<str>, phase: Phase, ctx: String, body: String, reply: oneshot::Sender<Outcome> },
}

pub struct ScriptPool {
    workers: Vec<mpsc::Sender<Job>>,
}

#[derive(Deserialize)]
struct RawOutcome {
    ops: Vec<Vec<String>>,
    headers: Vec<Vec<String>>,
    tests: Vec<(String, bool, Option<String>)>,
    error: Option<String>,
    logs: Vec<String>,
}

pub fn row_json(row: Option<&Row>) -> String {
    match row {
        Some(r) => serde_json::to_string(r.as_ref()).unwrap_or_else(|_| "null".into()),
        None => "null".into(),
    }
}

impl ScriptPool {
    /// Start `threads` engines. Returns the pool and compile errors (one per
    /// broken script) so they can be shown before the run starts.
    pub fn start(
        scripts: &[String],
        base: &BaseVars,
        threads: usize,
        timeout: Duration,
        verbose: bool,
    ) -> Result<(ScriptPool, Vec<String>)> {
        let base_json = serde_json::json!({
            "e": base.environment, "c": base.collection, "g": base.globals, "envName": base.environment_name,
        })
        .to_string();
        let scripts: Arc<Vec<String>> = Arc::new(scripts.to_vec());
        let mut workers = Vec::new();
        let mut compile_errors = Vec::new();
        for i in 0..threads.max(1) {
            let (tx, rx) = mpsc::channel::<Job>();
            let (ready_tx, ready_rx) = mpsc::channel::<Result<Vec<String>>>();
            let scripts = scripts.clone();
            let base_json = base_json.clone();
            std::thread::Builder::new()
                .name(format!("loadman-js-{i}"))
                .stack_size(4 * 1024 * 1024)
                .spawn(move || worker(rx, ready_tx, &scripts, &base_json, timeout, verbose))
                .context("cannot start script engine thread")?;
            let errors = ready_rx.recv().map_err(|_| anyhow!("script engine failed to start"))??;
            if i == 0 {
                compile_errors = errors;
            }
            workers.push(tx);
        }
        Ok((ScriptPool { workers }, compile_errors))
    }

    fn worker_for(&self, vu: usize) -> &mpsc::Sender<Job> {
        &self.workers[vu % self.workers.len()]
    }

    pub fn init_vu(&self, vu: usize, data: Option<&Row>) {
        let _ = self.worker_for(vu).send(Job::Init { vu, data: row_json(data) });
    }

    pub fn set_data(&self, vu: usize, data: Option<&Row>) {
        let _ = self.worker_for(vu).send(Job::SetData { vu, data: row_json(data) });
    }

    pub fn drop_vu(&self, vu: usize) {
        let _ = self.worker_for(vu).send(Job::Drop { vu });
    }

    /// Run the scripts `ids` (a JSON array) for one request phase.
    pub async fn exec(&self, vu: usize, ids: Arc<str>, phase: Phase, ctx: &ExecContext<'_>, body: String) -> Outcome {
        let mut json = serde_json::json!({
            "name": ctx.name, "id": ctx.id, "iteration": ctx.iteration, "method": ctx.method,
            "url": ctx.url, "reqHeaders": ctx.headers, "body": ctx.body,
        });
        if let Some(r) = &ctx.response {
            json["res"] = serde_json::json!({
                "code": r.code, "status": r.status, "time": r.time_ms, "size": r.size, "headers": r.headers,
            });
        }
        let (reply, rx) = oneshot::channel();
        let job = Job::Exec { vu, ids, phase, ctx: json.to_string(), body, reply };
        if self.worker_for(vu).send(job).is_err() {
            return Outcome { error: Some("InternalError: script engine stopped".into()), ..Outcome::default() };
        }
        rx.await.unwrap_or_else(|_| Outcome {
            error: Some("InternalError: script engine crashed".into()),
            ..Outcome::default()
        })
    }
}

fn worker(
    rx: mpsc::Receiver<Job>,
    ready: mpsc::Sender<Result<Vec<String>>>,
    scripts: &[String],
    base_json: &str,
    timeout: Duration,
    verbose: bool,
) {
    let clock = Instant::now();
    let deadline = Arc::new(AtomicU64::new(u64::MAX));
    let setup = || -> Result<(Runtime, Context, Vec<String>)> {
        let rt = Runtime::new()?;
        rt.set_memory_limit(512 * 1024 * 1024);
        rt.set_max_stack_size(2 * 1024 * 1024);
        let d = deadline.clone();
        rt.set_interrupt_handler(Some(Box::new(move || {
            clock.elapsed().as_millis() as u64 > d.load(Ordering::Relaxed)
        })));
        let ctx = Context::full(&rt)?;
        let errors = ctx.with(|ctx| -> Result<Vec<String>> {
            ctx.eval::<(), _>(PRELUDE).map_err(|e| anyhow!("prelude failed: {e}: {:?}", ctx.catch()))?;
            ctx.globals().set("__LM_VERBOSE", verbose)?;
            let set_base: Function = ctx.globals().get("__lm_setBase")?;
            set_base.call::<_, ()>((base_json,))?;
            let compile: Function = ctx.globals().get("__lm_compile")?;
            let mut errors = Vec::new();
            for (id, src) in scripts.iter().enumerate() {
                let msg: String = compile.call((id as i32, src.as_str()))?;
                if !msg.is_empty() {
                    errors.push(format!("script #{id}: {msg}"));
                }
            }
            Ok(errors)
        })?;
        Ok((rt, ctx, errors))
    };
    let (rt, ctx) = match setup() {
        Ok((rt, ctx, errors)) => {
            let _ = ready.send(Ok(errors));
            (rt, ctx)
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    let mut execs: u64 = 0;
    while let Ok(job) = rx.recv() {
        ctx.with(|ctx| match job {
            Job::Init { vu, data } => call_simple(&ctx, "__lm_initVu", vu, &data),
            Job::SetData { vu, data } => call_simple(&ctx, "__lm_setData", vu, &data),
            Job::Drop { vu } => call_simple(&ctx, "__lm_dropVu", vu, ""),
            Job::Exec { vu, ids, phase, ctx: json, body, reply } => {
                deadline.store(clock.elapsed().as_millis() as u64 + timeout.as_millis() as u64, Ordering::Relaxed);
                let phase = if phase == Phase::Pre { "pre" } else { "test" };
                let result = ctx
                    .globals()
                    .get::<_, Function>("__lm_exec")
                    .and_then(|f| f.call::<_, String>((vu as i32, &*ids, phase, json, body)));
                deadline.store(u64::MAX, Ordering::Relaxed);
                let outcome = match result {
                    Ok(s) => parse_outcome(&s),
                    Err(e) => {
                        let detail = ctx.catch();
                        let msg = detail
                            .as_object()
                            .and_then(|o| o.get::<_, String>("message").ok())
                            .unwrap_or_else(|| e.to_string());
                        let msg = if msg.contains("interrupted") {
                            format!("ScriptTimeout: script ran longer than {}ms", timeout.as_millis())
                        } else {
                            format!("InternalError: {msg}")
                        };
                        Outcome { error: Some(msg), ..Outcome::default() }
                    }
                };
                let _ = reply.send(outcome);
                execs += 1;
            }
        });
        if execs % 4096 == 4095 {
            rt.run_gc();
        }
    }
}

fn call_simple(ctx: &rquickjs::Ctx<'_>, name: &str, vu: usize, data: &str) {
    if let Ok(f) = ctx.globals().get::<_, Function>(name) {
        let _ = f.call::<_, ()>((vu as i32, data));
    }
}

fn parse_outcome(s: &str) -> Outcome {
    let raw: RawOutcome = match serde_json::from_str(s) {
        Ok(r) => r,
        Err(e) => {
            return Outcome { error: Some(format!("InternalError: bad script result: {e}")), ..Outcome::default() };
        }
    };
    let mut out = Outcome { error: raw.error, logs: raw.logs, ..Outcome::default() };
    for op in raw.ops {
        match op.as_slice() {
            [k] if k == "skip" => out.skip_request = true,
            [k, scope, key, value] if k == "s" => {
                if let Some(s) = Scope::from_code(scope) {
                    out.ops.push(VarOp::Set(s, key.clone(), value.clone()));
                }
            }
            [k, scope, key] if k == "u" => {
                if let Some(s) = Scope::from_code(scope) {
                    out.ops.push(VarOp::Unset(s, key.clone()));
                }
            }
            [k, scope] if k == "c" => {
                if let Some(s) = Scope::from_code(scope) {
                    out.ops.push(VarOp::Clear(s));
                }
            }
            _ => {}
        }
    }
    for h in raw.headers {
        match h.as_slice() {
            [k, name, value] if k == "set" => out.headers.push(HeaderOp::Set(name.clone(), value.clone())),
            [k, name, value] if k == "add" => out.headers.push(HeaderOp::Add(name.clone(), value.clone())),
            [k, name] if k == "rm" => out.headers.push(HeaderOp::Remove(name.clone())),
            _ => {}
        }
    }
    for (name, passed, msg) in raw.tests {
        if !passed && out.first_failure.is_none() {
            out.first_failure = Some(format!("{name}: {}", msg.unwrap_or_default()));
        }
        out.tests.push((name, passed));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(scripts: &[&str]) -> ScriptPool {
        let mut base = BaseVars::default();
        base.environment.insert("host".into(), "example.test".into());
        let scripts: Vec<String> = scripts.iter().map(|s| s.to_string()).collect();
        let (p, errs) = ScriptPool::start(&scripts, &base, 1, Duration::from_millis(500), false).unwrap();
        assert!(errs.is_empty(), "{errs:?}");
        p
    }

    fn ctx<'a>(headers: &'a [(String, String)], code: Option<u16>) -> ExecContext<'a> {
        ExecContext {
            name: "Get user",
            id: 0,
            iteration: 0,
            method: "GET",
            url: "http://example.test/u/1",
            headers,
            body: None,
            response: code.map(|c| ExecResponse {
                code: c,
                status: if c == 200 { "OK" } else { "Not Found" },
                time_ms: 12.5,
                size: 30,
                headers: vec![("Content-Type".into(), "application/json".into())],
            }),
        }
    }

    #[tokio::test]
    async fn tests_and_variable_ops() {
        let p = pool(&[r#"
            pm.test("status is 200", () => pm.response.to.have.status(200));
            pm.test("fast", () => pm.expect(pm.response.responseTime).to.be.below(100));
            pm.test("body", () => {
                const j = pm.response.json();
                pm.expect(j).to.have.property("id", 1);
                pm.expect(j.tags).to.include("a").and.have.lengthOf(2);
                pm.expect(j.name).to.be.a("string").that.matches(/^ad/);
            });
            pm.test("fails", () => pm.expect(pm.response.code).to.eql(201));
            pm.environment.set("token", pm.response.json().token);
            pm.collectionVariables.set("n", 5);
            tests["legacy"] = responseCode.code === 200;
        "#]);
        p.init_vu(0, None);
        let headers = vec![];
        let body = r#"{"id":1,"name":"ada","tags":["a","b"],"token":"t-123"}"#.to_string();
        let out = p.exec(0, "[0]".into(), Phase::Test, &ctx(&headers, Some(200)), body).await;
        assert_eq!(out.error, None);
        let results: Vec<_> = out.tests.iter().map(|(n, ok)| (n.as_str(), *ok)).collect();
        assert_eq!(
            results,
            [("status is 200", true), ("fast", true), ("body", true), ("fails", false), ("legacy", true)]
        );
        assert!(out.first_failure.unwrap().contains("expected 200 to deeply equal 201"));
        assert_eq!(out.ops[0], VarOp::Set(Scope::Environment, "token".into(), "t-123".into()));
        assert_eq!(out.ops[1], VarOp::Set(Scope::Collection, "n".into(), "5".into()));
    }

    #[tokio::test]
    async fn state_persists_per_vu_and_headers_can_change() {
        let p = pool(&[
            r#"const n = (pm.environment.get("count") || 0) + 1; pm.environment.set("count", n);
               pm.request.headers.upsert({key: "X-Count", value: String(n)});
               pm.request.headers.remove("X-Drop");"#,
            r#"pm.test("host from base", () => pm.expect(pm.variables.get("host")).to.equal("example.test"));
               pm.test("data", () => pm.expect(pm.iterationData.get("user")).to.equal("bob"));"#,
        ]);
        let mut row = std::collections::HashMap::new();
        row.insert("user".to_string(), "bob".to_string());
        let row: Row = Arc::new(row);
        p.init_vu(3, Some(&row));
        let headers = vec![("X-Drop".to_string(), "1".to_string())];
        let a = p.exec(3, "[0]".into(), Phase::Pre, &ctx(&headers, None), String::new()).await;
        let b = p.exec(3, "[0]".into(), Phase::Pre, &ctx(&headers, None), String::new()).await;
        assert_eq!(a.ops[0], VarOp::Set(Scope::Environment, "count".into(), "1".into()));
        assert_eq!(b.ops[0], VarOp::Set(Scope::Environment, "count".into(), "2".into()));
        assert_eq!(b.headers, vec![HeaderOp::Set("X-Count".into(), "2".into()), HeaderOp::Remove("X-Drop".into())]);
        let t = p.exec(3, "[1]".into(), Phase::Test, &ctx(&headers, Some(200)), "{}".into()).await;
        assert!(t.tests.iter().all(|(_, ok)| *ok), "{:?}", t.first_failure);
    }

    #[tokio::test]
    async fn errors_timeouts_and_compile_failures() {
        let mut base = BaseVars::default();
        base.globals.insert("g".into(), "1".into());
        let scripts =
            vec!["throw new TypeError('boom')".to_string(), "while (true) {}".to_string(), "let = ;".to_string()];
        let (p, errs) = ScriptPool::start(&scripts, &base, 1, Duration::from_millis(100), false).unwrap();
        assert_eq!(errs.len(), 1);
        let h = vec![];
        let e = p.exec(0, "[0]".into(), Phase::Test, &ctx(&h, Some(404)), String::new()).await;
        assert_eq!(e.error.as_deref(), Some("TypeError: boom"));
        let t = p.exec(0, "[1]".into(), Phase::Test, &ctx(&h, Some(200)), String::new()).await;
        assert!(t.error.unwrap().starts_with("ScriptTimeout"));
        let c = p.exec(0, "[2]".into(), Phase::Test, &ctx(&h, Some(200)), String::new()).await;
        assert!(c.error.unwrap().starts_with("SyntaxError"));
        // The engine keeps working after an interrupted script.
        let ok = p.exec(0, "[0]".into(), Phase::Test, &ctx(&h, Some(200)), String::new()).await;
        assert_eq!(ok.error.as_deref(), Some("TypeError: boom"));
    }
}
