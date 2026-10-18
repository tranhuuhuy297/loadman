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
    RunConfig {
        plan,
        profile: LoadProfile::named(ProfileKind::Fixed, vus, Duration::from_secs(secs), None).unwrap(),
        base,
        overrides,
        data: None,
        timeout: Duration::from_secs(5),
        script_timeout: Duration::from_secs(1),
        insecure: false,
        follow_redirects: true,
        think_time: None,
        tick: Duration::from_millis(250),
        scripts: true,
        script_threads: 2,
        workers: Some(2),
        fast_path: true,
        cookies: true,
        verbose: false,
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
    assert!(rep.assertions.iter().all(|a| a.failed == 0), "{:?}", rep.assertions);
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
              "const failed = await pm.sendRequest('http://127.0.0.1:9/').catch((e) => e);",
              "pm.test('errors reject', () => pm.expect(failed.code).to.eql('ECONNREFUSED'));"]}}]}]}"#,
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
