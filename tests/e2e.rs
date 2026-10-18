//! End-to-end runs against an in-process HTTP server.

mod support;

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use loadman::collection::{self, Selection};
use loadman::data::{DataSet, Distribution};
use loadman::engine::{self, RunConfig};
use loadman::profile::{LoadProfile, ProfileKind};
use loadman::report;
use loadman::threshold::Threshold;
use loadman::vars::BaseVars;

fn config(plan: collection::Plan, base_url: &str, vus: u32, secs: u64) -> RunConfig {
    let mut overrides = HashMap::new();
    overrides.insert("baseUrl".to_string(), base_url.to_string());
    let base = BaseVars {
        collection: plan.variables.clone(),
        environment: HashMap::from([("username".to_string(), "demo".to_string())]),
        ..BaseVars::default()
    };
    let profile = LoadProfile::named(ProfileKind::Fixed, vus, Duration::from_secs(secs), None).unwrap();
    RunConfig {
        base,
        overrides,
        timeout: Duration::from_secs(5),
        script_timeout: Duration::from_secs(1),
        tick: Duration::from_millis(250),
        script_threads: 2,
        workers: Some(2),
        ..RunConfig::new(plan, profile)
    }
}

fn demo_plan() -> collection::Plan {
    collection::load(Path::new("examples/demo/collection.json"), &Selection::default()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn demo_collection_runs_with_scripts_and_auth_chain() {
    let url = support::server::spawn().await;
    let plan = demo_plan();
    let cfg = config(plan.clone(), &url, 8, 2);
    let profile = cfg.profile.clone();
    let out = engine::run(cfg, |_| {}).await.unwrap();
    let thresholds = [Threshold::parse("error_rate<1%").unwrap(), Threshold::parse("requests>100").unwrap()];
    let rep = report::build(&plan, &profile, out, &thresholds);

    assert!(rep.totals.requests > 100, "only {} requests", rep.totals.requests);
    assert_eq!(rep.totals.transport_errors, 0);
    assert_eq!(rep.totals.script_errors, 0);
    // The token saved by the Login test script authorises Get user (401 otherwise).
    assert_eq!(rep.totals.http_errors, 0, "status codes: {:?}", rep.status_codes);
    assert_eq!(rep.assertions.len(), 5);
    for a in &rep.assertions {
        // "fast enough" checks response time < 200 ms, which a busy CI
        // machine misses now and then; everything else must always pass.
        if a.name == "fast enough" {
            assert!(a.failed * 100 <= a.passed, "{a:?}");
        } else {
            assert_eq!(a.failed, 0, "{a:?}");
        }
    }
    assert!(rep.passed());
    assert!(rep.iterations > 0);
    let per_request: Vec<u64> = rep.requests.iter().map(|r| r.totals.requests).collect();
    // Requests run in order, so counts differ by at most one per VU.
    assert!(per_request.windows(2).all(|w| w[0].abs_diff(w[1]) <= 8), "{per_request:?}");
    assert!(!rep.timeline.is_empty());
    let html = loadman::html::render(&rep);
    assert!(html.contains("Demo Shop API") && !html.contains("__DATA__"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_scripts_the_token_is_missing() {
    let url = support::server::spawn().await;
    let plan = demo_plan();
    let mut cfg = config(plan.clone(), &url, 2, 1);
    cfg.scripts = false;
    let profile = cfg.profile.clone();
    let rep = report::build(&plan, &profile, engine::run(cfg, |_| {}).await.unwrap(), &[]);
    let get_user = &rep.requests[1];
    assert_eq!(get_user.totals.http_errors, get_user.totals.requests);
    // HTTP errors are not transport/script errors (same definition as Postman's error rate).
    assert_eq!(rep.totals.error_rate, 0.0);
    assert!(rep.warnings.iter().any(|w| w.contains("--no-scripts")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transport_errors_are_classified() {
    // Bind and drop a listener to get a port that refuses connections.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let plan = demo_plan();
    let mut cfg = config(plan.clone(), &format!("http://127.0.0.1:{port}"), 2, 1);
    cfg.plan.requests.truncate(1);
    let profile = cfg.profile.clone();
    let rep = report::build(&cfg.plan.clone(), &profile, engine::run(cfg, |_| {}).await.unwrap(), &[]);
    assert!(rep.totals.requests > 0);
    assert_eq!(rep.totals.error_rate, 100.0);
    assert_eq!(rep.errors[0].0, "ECONNREFUSED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ramp_up_profile_adds_vus_and_data_rows_reach_requests() {
    let url = support::server::spawn().await;
    let plan = collection::parse(
        r#"{"info":{"name":"echo"},"item":[{"name":"echo","request":{"method":"GET",
            "url":"{{baseUrl}}/echo-header/x-user","header":[{"key":"X-User","value":"{{username}}"}]},
            "event":[{"listen":"test","script":{"exec":["pm.test('row', () => pm.expect(pm.response.text()).to.eql(pm.iterationData.get('username')))"]}}]}]}"#,
        Path::new("."),
        &Selection::default(),
    )
    .unwrap();
    let mut cfg = config(plan.clone(), &url, 6, 2);
    // 3s ramp-up: 1 VU until 0.75s, 6 VUs from 1.5s. The timeline keeps the
    // highest VU count seen in each second.
    cfg.profile = LoadProfile::named(ProfileKind::RampUp, 6, Duration::from_secs(3), Some(1)).unwrap();
    let dir = std::env::temp_dir().join(format!("loadman-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let csv = dir.join("users.csv");
    std::fs::write(&csv, "username\nada\ngrace\n").unwrap();
    cfg.data = Some(DataSet::load(&csv, Distribution::Sequential, false).unwrap());
    let profile = cfg.profile.clone();
    let rep = report::build(&plan, &profile, engine::run(cfg, |_| {}).await.unwrap(), &[]);
    assert!(rep.assertions[0].passed > 0 && rep.assertions[0].failed == 0, "{:?}", rep.assertions);
    let vus: Vec<u32> = rep.timeline.iter().map(|w| w.vus).collect();
    assert_eq!(vus.first(), Some(&1));
    assert_eq!(vus.iter().max(), Some(&6), "{vus:?}");
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn random_rows_change_between_iterations() {
    let url = support::server::spawn().await;
    // The test name carries the row, so the assertion list shows which rows one VU saw.
    let plan = collection::parse(
        r#"{"info":{"name":"rows"},"item":[{"name":"hello","request":{"method":"GET","url":"{{baseUrl}}/hello"},
            "event":[{"listen":"test","script":{"exec":["pm.test('row ' + pm.iterationData.get('username'), () => {})"]}}]}]}"#,
        Path::new("."),
        &Selection::default(),
    )
    .unwrap();
    let mut cfg = config(plan.clone(), &url, 1, 1);
    let dir = std::env::temp_dir().join(format!("loadman-random-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let csv = dir.join("users.csv");
    std::fs::write(&csv, "username\nada\ngrace\nlinus\n").unwrap();
    cfg.data = Some(DataSet::load(&csv, Distribution::Random, false).unwrap());
    let profile = cfg.profile.clone();
    let rep = report::build(&plan, &profile, engine::run(cfg, |_| {}).await.unwrap(), &[]);
    assert!(rep.iterations > 10, "{}", rep.iterations);
    assert!(rep.assertions.len() >= 2, "one VU only saw {:?}", rep.assertions);
    std::fs::remove_dir_all(dir).ok();
}

fn inline(json: &str) -> collection::Plan {
    collection::parse(json, Path::new("."), &Selection::default()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_request_from_scripts() {
    let url = support::server::spawn().await;
    // Callback style logs in before the request; await style fetches a value.
    let plan = inline(
        r#"{"info":{"name":"send"},"item":[
          {"name":"user","request":{"method":"GET","url":"{{baseUrl}}/users/1",
            "auth":{"type":"bearer","bearer":[{"key":"token","value":"{{token}}"}]}},
           "event":[
            {"listen":"prerequest","script":{"exec":[
              "pm.sendRequest({ url: pm.variables.get('baseUrl') + '/login', method: 'POST' }, (err, res) => {",
              "  pm.test('login answered', () => pm.expect(err).to.be.null && pm.expect(res).to.have.status(200));",
              "  pm.environment.set('token', res.json().token);",
              "});"]}},
            {"listen":"test","script":{"exec":[
              "pm.test('authorised', () => pm.response.to.have.status(200));",
              "const hello = await pm.sendRequest(pm.variables.get('baseUrl') + '/hello');",
              "pm.test('await style', () => pm.expect(hello.text()).to.eql('ok'));",
              "const failed = await pm.sendRequest('http://').catch((e) => e);",
              "pm.test('errors reject', () => pm.expect(failed.code).to.eql('EINVALIDURL'));"]}}]}]}"#,
    );
    let cfg = config(plan.clone(), &url, 2, 1);
    let profile = cfg.profile.clone();
    let rep = report::build(&plan, &profile, engine::run(cfg, |_| {}).await.unwrap(), &[]);
    assert!(rep.totals.requests > 0);
    assert_eq!((rep.totals.script_errors, rep.totals.http_errors), (0, 0), "{:?}", rep.script_errors);
    assert_eq!(rep.assertions.len(), 4, "{:?}", rep.assertions);
    assert!(rep.assertions.iter().all(|a| a.passed > 0 && a.failed == 0), "{:?}", rep.assertions);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cookies_persist_per_vu() {
    let url = support::server::spawn().await;
    let plan = inline(
        r#"{"info":{"name":"cookies"},"item":[
          {"name":"set","request":{"method":"GET","url":"{{baseUrl}}/cookie/set/session/abc"}},
          {"name":"sent back","request":{"method":"GET","url":"{{baseUrl}}/echo-header/cookie"},
           "event":[{"listen":"test","script":{"exec":[
             "pm.test('jar sends it', () => pm.expect(pm.response.text()).to.include('session=abc'));",
             "pm.test('pm.cookies sees it', () => pm.expect(pm.cookies.get('session')).to.eql('abc'));"]}}]},
          {"name":"redirect","request":{"method":"GET","url":"{{baseUrl}}/cookie/redirect"},
           "event":[{"listen":"test","script":{"exec":[
             "pm.test('cookie from the redirect', () => pm.expect(pm.response.text()).to.include('hop=1'));"]}}]}]}"#,
    );
    for (fast_path, cookies) in [(true, true), (false, true), (true, false)] {
        let mut cfg = config(plan.clone(), &url, 2, 1);
        cfg.fast_path = fast_path;
        cfg.cookies = cookies;
        let profile = cfg.profile.clone();
        let rep = report::build(&plan, &profile, engine::run(cfg, |_| {}).await.unwrap(), &[]);
        let all_passed = rep.assertions.iter().all(|a| a.passed > 0 && a.failed == 0);
        assert_eq!(all_passed, cookies, "fast path {fast_path}, cookies {cookies}: {:?}", rep.assertions);
        assert_eq!(rep.totals.error_rate, 0.0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_next_request_jumps_and_stops() {
    let url = support::server::spawn().await;
    // Each iteration runs first → third three times and then stops, so
    // "skipped" and "last" never run. Targets are looked up by folder path
    // and by item id.
    let plan = inline(
        r#"{"info":{"name":"flow"},"item":[
          {"id":"first-id","name":"first","request":{"method":"GET","url":"{{baseUrl}}/hello"},
           "event":[{"listen":"test","script":{"exec":["pm.execution.setNextRequest('Flow / third');"]}}]},
          {"name":"skipped","request":{"method":"GET","url":"{{baseUrl}}/hello"}},
          {"name":"Flow","item":[
            {"name":"third","request":{"method":"GET","url":"{{baseUrl}}/hello"},
             "event":[{"listen":"prerequest","script":{"exec":[
               "const n = (pm.environment.get('loops') || 0) + 1; pm.environment.set('loops', n);",
               "postman.setNextRequest(n % 3 ? 'first-id' : null);"]}}]}]},
          {"name":"last","request":{"method":"GET","url":"{{baseUrl}}/hello"}}]}"#,
    );
    let cfg = config(plan.clone(), &url, 1, 1);
    let profile = cfg.profile.clone();
    let rep = report::build(&plan, &profile, engine::run(cfg, |_| {}).await.unwrap(), &[]);
    let counts: Vec<u64> = rep.requests.iter().map(|r| r.totals.requests).collect();
    assert!(counts[0] >= 3, "{counts:?}");
    assert_eq!((counts[1], counts[3]), (0, 0), "{counts:?}");
    assert!(counts[2].abs_diff(counts[0]) <= 1, "{counts:?}");
    assert!(rep.iterations.abs_diff(counts[2] / 3) <= 1, "{} iterations, {counts:?}", rep.iterations);
    assert_eq!(rep.totals.script_errors, 0, "{:?}", rep.script_errors);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_request_form_data_and_file_bodies() {
    let url = support::server::spawn().await;
    let dir = std::env::temp_dir().join(format!("loadman-form-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("note.txt"), "file contents").unwrap();
    // File paths are relative to the collection's folder.
    let plan = collection::parse(
        r#"{"info":{"name":"form"},"item":[{"name":"hello","request":{"method":"GET","url":"{{baseUrl}}/hello"},
          "event":[{"listen":"test","script":{"exec":[
            "const echo = pm.variables.get('baseUrl') + '/echo';",
            "const form = await pm.sendRequest({ url: echo, method: 'POST', body: { mode: 'formdata', formdata: [",
            "  { key: 'greeting', value: 'hi', type: 'text' },",
            "  { key: 'upload', src: 'note.txt', type: 'file', contentType: 'text/plain' },",
            "  { key: 'off', value: 'x', disabled: true }] } });",
            "pm.test('multipart type', () => pm.expect(form.headers.get('content-type')).to.match(/^multipart\\/form-data; boundary=-+loadman/));",
            "pm.test('text field', () => pm.expect(form.text()).to.include('name=\"greeting\"\\r\\n\\r\\nhi\\r\\n'));",
            "pm.test('file field', () => pm.expect(form.text()).to.include('name=\"upload\"; filename=\"note.txt\"\\r\\nContent-Type: text/plain\\r\\n\\r\\nfile contents\\r\\n'));",
            "pm.test('disabled field', () => pm.expect(form.text()).to.not.include('name=\"off\"'));",
            "const bin = await pm.sendRequest({ url: echo, method: 'PUT', body: { mode: 'file', file: { src: 'note.txt' } } });",
            "pm.test('file body', () => pm.expect(bin.text()).to.eql('PUT file contents') && pm.expect(bin.headers.get('content-type')).to.eql('application/octet-stream'));",
            "const missing = await pm.sendRequest({ url: echo, method: 'POST', body: { mode: 'file', file: { src: 'nope.txt' } } }).catch((e) => e);",
            "pm.test('missing file', () => pm.expect(missing.code).to.eql('ENOENT'));"]}}]}]}"#,
        &dir,
        &Selection::default(),
    )
    .unwrap();
    let cfg = config(plan.clone(), &url, 1, 1);
    let profile = cfg.profile.clone();
    let rep = report::build(&plan, &profile, engine::run(cfg, |_| {}).await.unwrap(), &[]);
    assert_eq!(rep.totals.script_errors, 0, "{:?}", rep.script_errors);
    assert_eq!(rep.assertions.len(), 6, "{:?}", rep.assertions);
    assert!(rep.assertions.iter().all(|a| a.passed > 0 && a.failed == 0), "{:?}", rep.assertions);
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prometheus_scrape_and_push() {
    let url = support::server::spawn().await;
    let plan = inline(
        r#"{"info":{"name":"prom"},"item":[{"name":"hello","request":{"method":"GET","url":"{{baseUrl}}/hello"}}]}"#,
    );
    let addr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let job = format!("e2e-{}", std::process::id());
    let mut cfg = config(plan.clone(), &url, 2, 2);
    cfg.prometheus_listen = Some(addr);
    cfg.prometheus_push = Some(format!("{url}/metrics/job/{job}"));
    let scrape = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let r = reqwest::get(format!("http://{addr}/metrics")).await.unwrap();
        let ct = r.headers()["content-type"].to_str().unwrap().to_string();
        (ct, r.text().await.unwrap())
    });
    let out = engine::run(cfg, |_| {}).await.unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let total = out.aggregate.overall().count;
    let (ct, live) = scrape.await.unwrap();
    assert!(ct.starts_with("text/plain; version=0.0.4"), "{ct}");
    assert!(live.contains("loadman_vus 2\n"), "{live}");
    let line = live.lines().find(|l| l.starts_with("loadman_requests_total{")).unwrap();
    let live_count: u64 = line.rsplit(' ').next().unwrap().parse().unwrap();
    assert!(live_count > 0 && live_count < total, "{line} of {total}");
    // The last push carries the final totals.
    let pushed = support::server::pushed(&job);
    let last = pushed.last().expect("nothing pushed");
    assert!(last.contains(&format!("loadman_requests_total{{request=\"hello\",index=\"0\"}} {total}\n")), "{last}");
    assert!(last.contains("loadman_vus 0\n"));

    // A port that is taken fails the run before it starts.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut cfg = config(plan, &url, 1, 1);
    cfg.prometheus_listen = Some(taken.local_addr().unwrap());
    let err = engine::run(cfg, |_| {}).await.err().expect("bind should fail");
    assert!(format!("{err:#}").contains("--prometheus-listen"), "{err:#}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arrival_rate_starts_iterations_on_schedule() {
    let url = support::server::spawn().await;
    let plan = inline(
        r#"{"info":{"name":"open"},"item":[{"name":"hello","request":{"method":"GET","url":"{{baseUrl}}/hello"}}]}"#,
    );
    let mut cfg = config(plan.clone(), &url, 1, 2);
    cfg.arrival = Some(engine::ArrivalRate { peak_rate: 50.0, max_vus: 100 });
    let profile = cfg.profile.clone();
    let rep = report::build(&plan, &profile, engine::run(cfg, |_| {}).await.unwrap(), &[]);
    let a = rep.arrival.as_ref().unwrap();
    // 50/s for 2 s, whatever the response time.
    assert!((90..=101).contains(&rep.iterations), "{} iterations", rep.iterations);
    assert_eq!(a.dropped_iterations, 0);
    // Fast responses need only a few VUs (more on a busy CI machine).
    assert!((1..=30).contains(&a.peak_vus), "{a:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arrival_rate_drops_iterations_without_free_vus() {
    let url = support::server::spawn().await;
    let plan = inline(
        r#"{"info":{"name":"open"},"item":[{"name":"slow","request":{"method":"GET","url":"{{baseUrl}}/slow?ms=200"}}]}"#,
    );
    let mut cfg = config(plan.clone(), &url, 1, 2);
    // Two VUs finish about 10 iterations a second; 40 a second arrive.
    cfg.arrival = Some(engine::ArrivalRate { peak_rate: 40.0, max_vus: 2 });
    let profile = cfg.profile.clone();
    let thresholds = [Threshold::parse("dropped_iterations<1").unwrap()];
    let rep = report::build(&plan, &profile, engine::run(cfg, |_| {}).await.unwrap(), &thresholds);
    let a = rep.arrival.as_ref().unwrap();
    assert_eq!(a.peak_vus, 2);
    assert!(rep.iterations <= 24, "{} iterations", rep.iterations);
    assert!(a.dropped_iterations >= 50, "{a:?}");
    assert!((75..=81).contains(&(rep.iterations + a.dropped_iterations)), "{} + {a:?}", rep.iterations);
    assert!(!rep.passed());
    assert!(rep.timeline.iter().all(|w| w.vus <= 2), "{:?}", rep.timeline);
}
