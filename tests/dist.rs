//! Distributed runs: real agents on local ports, driven by the controller.

mod support;

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use loadman::collection::{self, Selection};
use loadman::data::Distribution;
use loadman::dist::{self, Job, JobData};
use loadman::engine::ArrivalRate;
use loadman::profile::{LoadProfile, ProfileKind};
use loadman::report;

const TOKEN: &str = "test-token-123";

async fn agents(n: usize) -> Vec<String> {
    let mut urls = Vec::new();
    for _ in 0..n {
        let l = dist::bind_agent("127.0.0.1:0".parse().unwrap()).await.unwrap();
        urls.push(format!("http://{}", l.local_addr().unwrap()));
        tokio::spawn(dist::serve_agent(l, TOKEN.into()));
    }
    urls
}

// Each VU posts a form-data file (sent along with the job) and names its
// data row in a test.
const COLLECTION: &str = r#"{"info":{"name":"dist"},"item":[{"name":"upload","request":{"method":"POST",
  "url":"{{baseUrl}}/echo","body":{"mode":"formdata","formdata":[{"key":"f","type":"file","src":"note.txt"}]}},
  "event":[{"listen":"test","script":{"exec":[
    "pm.test('file arrived', () => pm.expect(pm.response.text()).to.include('file contents'));",
    "pm.test('row ' + pm.iterationData.get('user'), () => {});"]}}]}]}"#;

/// The collection's folder, with the file it uploads. Written once: tests
/// run in parallel, and rewriting it could let another test read it empty.
fn dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("loadman-dist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("note.txt"), "file contents").unwrap();
        dir
    })
    .clone()
}

fn job(url: &str, profile: LoadProfile) -> Job {
    let value: serde_json::Value = serde_json::from_str(COLLECTION).unwrap();
    Job {
        files: dist::bundle_files(&value, &dir()).unwrap(),
        overrides: [("baseUrl".to_string(), url.to_string())].into(),
        data: Some(JobData {
            text: "user\na\nb\nc\nd\ne\n".into(),
            json: false,
            distribution: Distribution::Ordered,
            per_iteration: false,
        }),
        workers: Some(1),
        script_threads: Some(1),
        ..Job::new(COLLECTION.into(), profile)
    }
}

fn plan() -> collection::Plan {
    collection::parse(COLLECTION, &dir(), &Selection::default()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_split_the_load_and_the_report_adds_up() {
    let url = support::server::spawn().await;
    let agents = agents(2).await;
    let profile = LoadProfile::named(ProfileKind::Fixed, 5, Duration::from_secs(2), None).unwrap();
    let mut seen = Vec::new();
    let out = dist::run_on_agents(&agents, TOKEN, job(&url, profile.clone()), |p| seen.push((p.vus, p.total_requests)))
        .await
        .unwrap();
    assert!(!out.interrupted && out.warnings.is_empty(), "{:?}", out.warnings);
    let total = out.aggregate.overall().count;
    let rep = report::build(&plan(), &profile, out, &[]);
    assert_eq!(rep.totals.script_errors + rep.totals.transport_errors, 0, "{:?}", rep.script_errors);
    // 5 VUs over two agents (3 + 2), each with its own row: VU k gets row k.
    let mut names: Vec<_> = rep.assertions.iter().map(|a| a.name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["file arrived", "row a", "row b", "row c", "row d", "row e"], "{:?}", rep.assertions);
    assert!(
        rep.assertions.iter().all(|a| a.failed == 0 && a.passed > 0),
        "{:?}; {} requests, status {:?}, errors {:?}",
        rep.assertions,
        rep.totals.requests,
        rep.status_codes,
        rep.errors
    );
    // The merged timeline and the totals tell the same story.
    let in_timeline: u64 = rep.timeline.iter().map(|w| w.requests).sum();
    assert!(in_timeline <= total && in_timeline * 10 >= total * 5, "{in_timeline} of {total}");
    assert_eq!(rep.timeline.iter().map(|w| w.vus).max(), Some(5), "{:?}", rep.timeline);
    // Progress came in with the whole run's VUs.
    assert!(seen.iter().any(|(vus, n)| *vus == 5 && *n > 0), "{seen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn arrival_rate_is_shared_between_agents() {
    let url = support::server::spawn().await;
    let agents = agents(2).await;
    let profile = LoadProfile::named(ProfileKind::Fixed, 1, Duration::from_secs(2), None).unwrap();
    let mut job = job(&url, profile.clone());
    job.data = None;
    job.arrival = Some(ArrivalRate { peak_rate: 40.0, max_vus: 10 });
    let out = dist::run_on_agents(&agents, TOKEN, job, |_| {}).await.unwrap();
    // 20/s on each agent for 2 s.
    assert!((76..=81).contains(&out.iterations), "{} iterations", out.iterations);
    assert_eq!(out.dropped_iterations, 0);
    assert_eq!(out.arrival.map(|a| a.peak_rate), Some(40.0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_check_the_token_and_can_be_stopped() {
    let url = support::server::spawn().await;
    let agents = agents(1).await;
    let profile = LoadProfile::named(ProfileKind::Fixed, 1, Duration::from_secs(30), None).unwrap();
    let err = dist::run_on_agents(&agents, "wrong-token", job(&url, profile.clone()), |_| {}).await.err().unwrap();
    assert!(format!("{err:#}").contains("token"), "{err:#}");
    let mut stale = job(&url, profile.clone());
    stale.version = "0.0.1".into();
    let r = reqwest::Client::new()
        .post(format!("{}/v1/run", agents[0]))
        .bearer_auth(TOKEN)
        .body(serde_json::to_vec(&stale).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("0.0.1"));

    // A 30 s test, stopped after a second, still reports what it did.
    let stop_url = format!("{}/v1/stop", agents[0]);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        reqwest::Client::new().post(stop_url).bearer_auth(TOKEN).send().await.unwrap();
    });
    let started = std::time::Instant::now();
    let out = dist::run_on_agents(&agents, TOKEN, job(&url, profile), |_| {}).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    assert!(out.interrupted && out.aggregate.overall().count > 0);
}
