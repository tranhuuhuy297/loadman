use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde_json::Value;

use loadman::collection::{self, AuthSpec, BodySpec, Selection};
use loadman::data::{DataSet, Distribution};
use loadman::engine::{self, Progress, RunConfig};
use loadman::profile::{LoadProfile, ProfileKind, parse_duration};
use loadman::report::{self, Style, thousands};
use loadman::threshold::Threshold;
use loadman::vars::{BaseVars, json_to_string};
use loadman::{html, template::Template};

#[derive(Parser)]
#[command(name = "loadman", version, about = "Load testing for Postman collections, from your terminal or CI.")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Cmd {
    /// Run a performance test against a collection.
    Run(RunArgs),
    /// List the requests a run would execute, in order, with what is supported.
    Inspect {
        collection: PathBuf,
        #[command(flatten)]
        select: SelectArgs,
    },
    /// Preview a load profile without sending requests.
    Profile(ProfileArgs),
}

#[derive(Args)]
struct SelectArgs {
    /// Only run requests inside this folder (repeatable).
    #[arg(long = "folder", value_name = "NAME")]
    folders: Vec<String>,
    /// Only run requests with this name (repeatable).
    #[arg(long = "request", value_name = "NAME")]
    requests: Vec<String>,
}

#[derive(Args, Clone)]
struct ProfileArgs {
    /// Load profile shape.
    #[arg(short, long, value_enum, default_value = "fixed")]
    profile: ProfileKind,
    /// Maximum number of virtual users.
    #[arg(short = 'u', long, default_value_t = 20)]
    vus: u32,
    /// Total test duration (e.g. 30s, 5m, 1h).
    #[arg(short = 't', long, default_value = "1m", value_parser = parse_duration)]
    duration: Duration,
    /// Base load for ramp-up, spike and peak (default: 25%, 10%, 20% of --vus).
    #[arg(long, value_name = "N")]
    base_vus: Option<u32>,
    /// Custom control points as absolute `time:vus` pairs, e.g. "0:5,30s:50,4m:50,5m:0".
    #[arg(long, value_name = "SPEC", conflicts_with = "load_profile")]
    stages: Option<String>,
    /// Load profile JSON in the Postman shape ({type, options.totalDuration, controlPoints}).
    #[arg(long, value_name = "FILE")]
    load_profile: Option<PathBuf>,
}

impl ProfileArgs {
    fn build(&self) -> Result<LoadProfile> {
        if let Some(spec) = &self.stages {
            return LoadProfile::from_stages(spec);
        }
        if let Some(path) = &self.load_profile {
            let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
            let v: Value = serde_json::from_str(&text).context("load profile is not valid JSON")?;
            return LoadProfile::from_postman_json(v.get("loadProfile").unwrap_or(&v));
        }
        LoadProfile::named(self.profile, self.vus, self.duration, self.base_vus)
    }
}

#[derive(Args)]
struct RunArgs {
    /// Postman collection (v2.0 or v2.1 JSON export).
    collection: PathBuf,
    #[command(flatten)]
    load: ProfileArgs,
    #[command(flatten)]
    select: SelectArgs,
    /// Postman environment export.
    #[arg(short, long, value_name = "FILE")]
    environment: Option<PathBuf>,
    /// Postman globals export.
    #[arg(short, long, value_name = "FILE")]
    globals: Option<PathBuf>,
    /// Set an environment variable, overriding the environment file (repeatable).
    #[arg(long = "env-var", value_name = "KEY=VALUE")]
    env_vars: Vec<String>,
    /// CSV or JSON data file; each virtual user gets rows from it.
    #[arg(short = 'd', long, value_name = "FILE")]
    iteration_data: Option<PathBuf>,
    /// How data rows are assigned to virtual users.
    #[arg(long, value_enum, default_value = "sequential")]
    data_distribution: Distribution,
    /// Pick a new data row on every iteration instead of once per virtual user.
    #[arg(long)]
    data_per_iteration: bool,
    /// Pass/fail rule, e.g. "p95<300ms", "error_rate<1%", "rps>500" (repeatable).
    #[arg(long = "threshold", value_name = "RULE")]
    thresholds: Vec<String>,
    /// Write the full report as JSON.
    #[arg(short, long, value_name = "FILE")]
    out: Option<PathBuf>,
    /// Write a self-contained HTML report.
    #[arg(long, value_name = "FILE")]
    html: Option<PathBuf>,
    /// Per-request timeout.
    #[arg(long, default_value = "30s", value_parser = parse_duration)]
    timeout: Duration,
    /// Maximum run time of one pre-request or test script.
    #[arg(long, default_value = "5s", value_parser = parse_duration)]
    script_timeout: Duration,
    /// Pause after each request: fixed ("200ms") or random range ("100ms-1s").
    #[arg(long, value_name = "DURATION")]
    think_time: Option<String>,
    /// Do not run pre-request and test scripts.
    #[arg(long)]
    no_scripts: bool,
    /// Script engine threads (default: half the CPU cores).
    #[arg(long, value_name = "N")]
    script_threads: Option<usize>,
    /// HTTP worker threads (default: one per CPU core).
    #[arg(long, value_name = "N")]
    workers: Option<usize>,
    /// Accept invalid TLS certificates.
    #[arg(short = 'k', long)]
    insecure: bool,
    /// Do not follow redirects.
    #[arg(long)]
    no_redirects: bool,
    /// Send http:// and https:// requests through the shared connection pool
    /// instead of connections owned by each VU (slower; for comparison).
    #[arg(long)]
    no_fast_path: bool,
    /// How often the VU count is adjusted to the profile.
    #[arg(long, default_value = "1s", value_parser = parse_duration, hide = true)]
    tick: Duration,
    /// Print failed requests, failed tests and script console output.
    #[arg(short, long)]
    verbose: bool,
    /// No live progress line.
    #[arg(short, long)]
    quiet: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Run(args) => run(args),
        Cmd::Inspect { collection, select } => inspect(&collection, &select).map(|_| true),
        Cmd::Profile(p) => preview_profile(&p).map(|_| true),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn selection(s: &SelectArgs) -> Selection {
    Selection { folders: s.folders.clone(), requests: s.requests.clone() }
}

/// Read `values` from a Postman environment or globals export.
fn read_variable_file(path: &Path) -> Result<(HashMap<String, String>, Option<String>)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let v: Value = serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", path.display()))?;
    let v = v.get("environment").unwrap_or(&v);
    let values = v
        .get("values")
        .and_then(Value::as_array)
        .with_context(|| format!("{} has no `values` array (expected a Postman export)", path.display()))?;
    let mut out = HashMap::new();
    for item in values {
        if item.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        if let Some(k) = item.get("key").and_then(Value::as_str) {
            out.insert(k.to_string(), item.get("value").map(json_to_string).unwrap_or_default());
        }
    }
    Ok((out, v.get("name").and_then(Value::as_str).map(String::from)))
}

fn parse_think_time(s: &str) -> Result<(Duration, Duration)> {
    match s.split_once('-') {
        Some((a, b)) => {
            let (a, b) = (parse_duration(a.trim())?, parse_duration(b.trim())?);
            if b < a {
                bail!("--think-time range must be low-high");
            }
            Ok((a, b))
        }
        None => {
            let d = parse_duration(s.trim())?;
            Ok((d, d))
        }
    }
}

fn run(a: RunArgs) -> Result<bool> {
    let plan = collection::load(&a.collection, &selection(&a.select))?;
    let profile = a.load.build()?;
    let thresholds = a.thresholds.iter().map(|t| Threshold::parse(t)).collect::<Result<Vec<_>>>()?;

    let mut base = BaseVars { collection: plan.variables.clone(), ..BaseVars::default() };
    if let Some(p) = &a.environment {
        let (vars, name) = read_variable_file(p)?;
        base.environment = vars;
        base.environment_name = name;
    }
    if let Some(p) = &a.globals {
        base.globals = read_variable_file(p)?.0;
    }
    let mut overrides = HashMap::new();
    for kv in &a.env_vars {
        let (k, v) = kv.split_once('=').with_context(|| format!("--env-var `{kv}` must be KEY=VALUE"))?;
        overrides.insert(k.to_string(), v.to_string());
    }
    let data = match &a.iteration_data {
        Some(p) => Some(DataSet::load(p, a.data_distribution, a.data_per_iteration)?),
        None => None,
    };
    let think_time = a.think_time.as_deref().map(parse_think_time).transpose()?;
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());

    engine::raise_fd_limit();
    let st = Style::detect();
    let quiet = a.quiet;
    let tty = std::io::stderr().is_terminal();
    eprintln!(
        "{} {} · {} requests · {} profile · up to {} VUs · {}",
        st.bold("loadman"),
        plan.name,
        plan.requests.len(),
        profile.kind.label(),
        profile.max_vus(),
        humantime::format_duration(profile.duration())
    );

    let cfg = RunConfig {
        plan: plan.clone(),
        profile: profile.clone(),
        base,
        overrides,
        data,
        timeout: a.timeout,
        script_timeout: a.script_timeout,
        insecure: a.insecure,
        follow_redirects: !a.no_redirects,
        think_time,
        tick: a.tick,
        scripts: !a.no_scripts,
        script_threads: a.script_threads.unwrap_or((cpus / 2).max(1)),
        workers: a.workers,
        fast_path: !a.no_fast_path,
        verbose: a.verbose,
    };

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let mut last_print = 0u64;
    let out = rt.block_on(engine::run(cfg, |p: &Progress| {
        if quiet {
            return;
        }
        let secs = p.elapsed.as_secs();
        let line = progress_line(p);
        if tty && !a.verbose {
            eprint!("\r\x1b[2K{line}");
            let _ = std::io::stderr().flush();
        } else if secs >= last_print + 10 {
            last_print = secs;
            eprintln!("{line}");
        }
    }))?;
    if tty && !quiet && !a.verbose {
        eprint!("\r\x1b[2K");
    }
    rt.shutdown_timeout(Duration::from_secs(1));

    let rep = report::build(&plan, &profile, out, &thresholds);
    print!("{}", report::render_terminal(&rep, &st));
    if let Some(p) = &a.out {
        std::fs::write(p, serde_json::to_vec_pretty(&rep)?).with_context(|| format!("cannot write {}", p.display()))?;
        println!("\n  JSON report → {}", p.display());
    }
    if let Some(p) = &a.html {
        std::fs::write(p, html::render(&rep)).with_context(|| format!("cannot write {}", p.display()))?;
        println!("  HTML report → {}", p.display());
    }
    println!();
    Ok(rep.passed())
}

fn progress_line(p: &Progress) -> String {
    let pct = (p.elapsed.as_secs_f64() / p.duration.as_secs_f64() * 100.0).min(100.0);
    let mut s = format!(
        "[{:>5.1}%] {:>4}s/{}s  VUs {:>4}  req {:>10}",
        pct,
        p.elapsed.as_secs(),
        p.duration.as_secs(),
        p.vus,
        thousands(p.total_requests)
    );
    if let Some(w) = p.last_second {
        s.push_str(&format!(
            "  {:>8}/s  avg {:>7}  p95 {:>7}",
            thousands(w.requests),
            report::ms(w.avg_ms),
            report::ms(w.p95_ms)
        ));
    }
    let err = if p.total_requests == 0 { 0.0 } else { p.total_errors as f64 * 100.0 / p.total_requests as f64 };
    s.push_str(&format!("  err {err:.2}%"));
    s
}

fn inspect(path: &Path, select: &SelectArgs) -> Result<()> {
    let plan = collection::load(path, &selection(select))?;
    let st = Style::detect();
    println!("{} · {} requests · {} scripts", st.bold(&plan.name), plan.requests.len(), plan.scripts.len());
    let mut referenced: Vec<String> = Vec::new();
    let mut note = |t: &Template| {
        for v in t.variables() {
            if !v.starts_with('$') && !referenced.iter().any(|r| r == v) {
                referenced.push(v.to_string());
            }
        }
    };
    for r in &plan.requests {
        let auth = match &r.auth {
            AuthSpec::None => "",
            AuthSpec::Bearer(_) => " · bearer",
            AuthSpec::Basic { .. } => " · basic",
            AuthSpec::ApiKey { .. } => " · api key",
        };
        let body = match &r.body {
            BodySpec::None => "",
            BodySpec::Raw { .. } => " · raw body",
            BodySpec::UrlEncoded(_) => " · urlencoded",
            BodySpec::Multipart(_) => " · form-data",
            BodySpec::Bytes { .. } => " · file body",
            BodySpec::Graphql { .. } => " · graphql",
        };
        let scripts = match (r.pre_scripts.len(), r.test_scripts.len()) {
            (0, 0) => String::new(),
            (p, t) => format!(" · scripts {p} pre / {t} test"),
        };
        println!("  {:>2}. {:<7} {}{}{}{}", r.index + 1, r.method, r.path, auth, body, scripts);
        println!("      {}", st.dim(&r.url.source()));
        note(&r.url);
        for (k, v) in &r.headers {
            note(k);
            note(v);
        }
        if let BodySpec::Raw { text, .. } = &r.body {
            note(text);
        }
    }
    let undefined: Vec<_> = referenced.iter().filter(|v| !plan.variables.contains_key(*v)).cloned().collect();
    if !undefined.is_empty() {
        println!(
            "\n  {} variables not defined in the collection (supply them with -e, -d or --env-var): {}",
            st.yellow("!"),
            undefined.join(", ")
        );
    }
    for w in &plan.warnings {
        println!("  {} {w}", st.yellow("!"));
    }
    Ok(())
}

fn preview_profile(p: &ProfileArgs) -> Result<()> {
    let profile = p.build()?;
    let st = Style::detect();
    println!(
        "{} profile · max {} VUs · {} · {:.2} VU-hours",
        st.bold(profile.kind.label()),
        profile.max_vus(),
        humantime::format_duration(profile.duration()),
        profile.vu_hours()
    );
    println!("  {}", profile.sparkline(60));
    for c in &profile.points {
        println!(
            "  {:>10}  {:>6} VUs",
            humantime::format_duration(Duration::from_millis(c.time_ms)).to_string(),
            c.vus
        );
    }
    Ok(())
}
