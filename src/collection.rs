//! Postman Collection v2.0 / v2.1 parsing (format documented at
//! https://schema.postman.com) and flattening into an execution plan.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::template::Template;
use crate::vars::json_to_string;

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawCollection {
    info: RawInfo,
    #[serde(default)]
    item: Vec<RawItem>,
    #[serde(default)]
    variable: Vec<RawVariable>,
    auth: Option<RawAuth>,
    #[serde(default)]
    event: Vec<RawEvent>,
}

#[derive(Debug, Deserialize)]
struct RawInfo {
    name: String,
}

#[derive(Debug, Deserialize)]
struct RawItem {
    id: Option<String>,
    name: Option<String>,
    item: Option<Vec<RawItem>>,
    request: Option<RawRequestDef>,
    #[serde(default)]
    event: Vec<RawEvent>,
    auth: Option<RawAuth>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawRequestDef {
    Url(String),
    Full(Box<RawRequest>),
}

#[derive(Debug, Deserialize)]
struct RawRequest {
    method: Option<String>,
    url: Option<RawUrl>,
    header: Option<RawHeaders>,
    body: Option<RawBody>,
    auth: Option<RawAuth>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawUrl {
    Raw(String),
    Obj(RawUrlObj),
}

#[derive(Debug, Deserialize)]
struct RawUrlObj {
    raw: Option<String>,
    protocol: Option<String>,
    host: Option<StrOrList>,
    port: Option<String>,
    path: Option<PathParts>,
    #[serde(default)]
    query: Vec<RawKv>,
    /// Values for `:name` path segments.
    #[serde(default)]
    variable: Vec<RawKv>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StrOrList {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PathParts {
    One(String),
    Many(Vec<Value>),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawHeaders {
    List(Vec<RawKv>),
    Text(String),
}

#[derive(Debug, Deserialize)]
struct RawKv {
    key: Option<String>,
    value: Option<Value>,
    #[serde(default)]
    disabled: bool,
}

#[derive(Debug, Deserialize)]
struct RawBody {
    mode: Option<String>,
    raw: Option<String>,
    #[serde(default)]
    urlencoded: Vec<RawKv>,
    #[serde(default)]
    formdata: Vec<RawFormParam>,
    file: Option<RawFile>,
    graphql: Option<RawGraphql>,
    options: Option<Value>,
    #[serde(default)]
    disabled: bool,
}

#[derive(Debug, Deserialize)]
struct RawFormParam {
    key: Option<String>,
    value: Option<Value>,
    src: Option<Value>,
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "contentType")]
    content_type: Option<String>,
    #[serde(default)]
    disabled: bool,
}

#[derive(Debug, Deserialize)]
struct RawFile {
    src: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawGraphql {
    query: Option<String>,
    variables: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct RawAuth {
    #[serde(rename = "type")]
    kind: String,
    #[serde(flatten)]
    params: HashMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct RawEvent {
    listen: String,
    script: Option<RawScript>,
    #[serde(default)]
    disabled: bool,
}

#[derive(Debug, Deserialize)]
struct RawScript {
    exec: Option<StrOrList>,
}

#[derive(Debug, Deserialize)]
struct RawVariable {
    key: Option<String>,
    id: Option<String>,
    value: Option<Value>,
    #[serde(default)]
    disabled: bool,
}

// ---------------------------------------------------------------------------
// Execution plan
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum BodySpec {
    None,
    Raw { text: Template, content_type: Option<&'static str> },
    UrlEncoded(Vec<(Template, Template)>),
    Multipart(Vec<FormPart>),
    Bytes { data: bytes::Bytes, content_type: Option<String> },
    Graphql { query: Template, variables: Template },
}

#[derive(Debug, Clone)]
pub enum FormPart {
    Text { name: Template, value: Template, content_type: Option<String> },
    File { name: Template, file_name: String, data: bytes::Bytes, content_type: Option<String> },
}

#[derive(Debug, Clone)]
pub enum AuthSpec {
    None,
    Bearer(Template),
    Basic { user: Template, pass: Template },
    ApiKey { key: Template, value: Template, in_query: bool },
}

#[derive(Debug, Clone)]
pub struct RequestSpec {
    pub index: usize,
    /// The item's `id` in the collection file, when it has one.
    pub item_id: Option<String>,
    pub name: String,
    /// Folder path plus request name, e.g. `Auth / Login`.
    pub path: String,
    pub method: String,
    pub url: Template,
    pub headers: Vec<(Template, Template)>,
    pub body: BodySpec,
    pub auth: AuthSpec,
    /// Indices into [`Plan::scripts`], outermost (collection) first.
    pub pre_scripts: Vec<usize>,
    pub test_scripts: Vec<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub name: String,
    pub requests: Vec<RequestSpec>,
    pub scripts: Vec<String>,
    pub variables: HashMap<String, String>,
    pub warnings: Vec<String>,
    /// The collection's folder; file paths in bodies are relative to it.
    pub base_dir: PathBuf,
    /// Set for a collection received with its files (a distributed run's
    /// agent): bodies read files from here, never from the disk.
    pub files: Option<Arc<Files>>,
}

/// File contents by the path the collection uses for them.
pub type Files = HashMap<String, bytes::Bytes>;

/// Which requests to keep (Postman calls this the request selection).
#[derive(Debug, Default, Clone)]
pub struct Selection {
    pub folders: Vec<String>,
    pub requests: Vec<String>,
}

impl Selection {
    fn keeps(&self, folders: &[String], name: &str) -> bool {
        let folder_ok = self.folders.is_empty() || folders.iter().any(|f| self.folders.contains(f));
        let request_ok = self.requests.is_empty() || self.requests.iter().any(|r| r == name);
        folder_ok && request_ok
    }
}

pub fn load(path: &Path, selection: &Selection) -> Result<Plan> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read collection {}", path.display()))?;
    let base_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    parse(&text, &base_dir, selection)
}

/// Parse a collection whose body files come with it instead of from disk.
pub fn parse_bundle(text: &str, files: Files, selection: &Selection) -> Result<Plan> {
    parse_inner(text, Path::new(""), Some(Arc::new(files)), selection)
}

pub fn parse(text: &str, base_dir: &Path, selection: &Selection) -> Result<Plan> {
    parse_inner(text, base_dir, None, selection)
}

fn parse_inner(text: &str, base_dir: &Path, files: Option<Arc<Files>>, selection: &Selection) -> Result<Plan> {
    let mut value: Value = serde_json::from_str(text).context("collection is not valid JSON")?;
    // Exports from the Postman API wrap the collection in {"collection": {...}}.
    if let Some(inner) = value.get_mut("collection").map(Value::take) {
        value = inner;
    }
    if value.get("info").is_none() {
        bail!("not a Postman v2 collection (missing `info`); v1 collections must be re-exported as v2.1");
    }
    let raw: RawCollection = serde_json::from_value(value).context("unsupported collection layout")?;

    let mut b = Builder {
        plan: Plan { name: raw.info.name.clone(), base_dir: base_dir.to_path_buf(), files, ..Plan::default() },
        script_ids: HashMap::new(),
        base_dir: base_dir.to_path_buf(),
        selection,
    };
    for v in raw.variable.iter().filter(|v| !v.disabled) {
        if let Some(k) = v.key.as_ref().or(v.id.as_ref()) {
            b.plan.variables.insert(k.clone(), v.value.as_ref().map(json_to_string).unwrap_or_default());
        }
    }
    let ctx = Inherited {
        auth: raw.auth.clone(),
        pre: b.scripts_for(&raw.event, "prerequest"),
        test: b.scripts_for(&raw.event, "test"),
        folders: Vec::new(),
    };
    b.walk(&raw.item, &ctx)?;
    if b.plan.requests.is_empty() {
        bail!("the collection (after --folder/--request filters) contains no requests");
    }
    Ok(b.plan)
}

#[derive(Clone)]
struct Inherited {
    auth: Option<RawAuth>,
    pre: Vec<usize>,
    test: Vec<usize>,
    folders: Vec<String>,
}

struct Builder<'a> {
    plan: Plan,
    script_ids: HashMap<String, usize>,
    base_dir: PathBuf,
    selection: &'a Selection,
}

impl Builder<'_> {
    fn read_file(&self, src: &str) -> Result<bytes::Bytes> {
        if let Some(files) = &self.plan.files {
            return files.get(src).cloned().with_context(|| format!("file `{src}` was not sent with the collection"));
        }
        let path = self.base_dir.join(src);
        Ok(std::fs::read(&path)
            .with_context(|| format!("file `{}` (paths are relative to the collection)", path.display()))?
            .into())
    }

    fn warn(&mut self, msg: String) {
        if !self.plan.warnings.contains(&msg) {
            self.plan.warnings.push(msg);
        }
    }

    fn scripts_for(&mut self, events: &[RawEvent], listen: &str) -> Vec<usize> {
        let mut out = Vec::new();
        for e in events.iter().filter(|e| e.listen == listen && !e.disabled) {
            let src = match e.script.as_ref().and_then(|s| s.exec.as_ref()) {
                Some(StrOrList::One(s)) => s.clone(),
                Some(StrOrList::Many(lines)) => lines.join("\n"),
                None => continue,
            };
            if src.trim().is_empty() {
                continue;
            }
            let next = self.plan.scripts.len();
            let id = *self.script_ids.entry(src.clone()).or_insert(next);
            if id == next {
                self.plan.scripts.push(src);
            }
            out.push(id);
        }
        out
    }

    fn walk(&mut self, items: &[RawItem], ctx: &Inherited) -> Result<()> {
        for item in items {
            let name = item.name.clone().unwrap_or_else(|| "Untitled".into());
            let mut inner = ctx.clone();
            if let Some(a) = &item.auth
                && a.kind != "inherit"
            {
                inner.auth = Some(a.clone());
            }
            inner.pre.extend(self.scripts_for(&item.event, "prerequest"));
            inner.test.extend(self.scripts_for(&item.event, "test"));
            if let Some(children) = &item.item {
                inner.folders.push(name);
                self.walk(children, &inner)?;
            } else if let Some(req) = &item.request
                && self.selection.keeps(&ctx.folders, &name)
            {
                let mut spec = self.request(req, &name, &inner).with_context(|| format!("request `{name}`"))?;
                spec.item_id = item.id.clone();
                self.plan.requests.push(spec);
            }
        }
        Ok(())
    }

    fn request(&mut self, def: &RawRequestDef, name: &str, ctx: &Inherited) -> Result<RequestSpec> {
        let empty = RawRequest { method: None, url: None, header: None, body: None, auth: None };
        let (req, url_string) = match def {
            RawRequestDef::Url(u) => (&empty, u.clone()),
            RawRequestDef::Full(r) => (r.as_ref(), r.url.as_ref().map(url_to_string).unwrap_or_default()),
        };
        let method = req.method.clone().unwrap_or_else(|| "GET".into()).to_uppercase();

        let mut headers = Vec::new();
        match &req.header {
            Some(RawHeaders::List(list)) => {
                for h in list.iter().filter(|h| !h.disabled) {
                    if let Some(k) = h.key.as_deref().filter(|k| !k.is_empty()) {
                        let v = h.value.as_ref().map(json_to_string).unwrap_or_default();
                        headers.push((Template::parse(k), Template::parse(&v)));
                    }
                }
            }
            Some(RawHeaders::Text(text)) => {
                for line in text.lines() {
                    if let Some((k, v)) = line.split_once(':') {
                        headers.push((Template::parse(k.trim()), Template::parse(v.trim())));
                    }
                }
            }
            None => {}
        }

        let auth_raw = match &req.auth {
            Some(a) if a.kind != "inherit" => Some(a.clone()),
            _ => ctx.auth.clone(),
        };
        let auth = self.auth(auth_raw.as_ref(), name);
        let body = match &req.body {
            Some(b) if !b.disabled => self.body(b, name)?,
            _ => BodySpec::None,
        };

        let mut folders = ctx.folders.clone();
        folders.push(name.to_string());
        Ok(RequestSpec {
            index: self.plan.requests.len(),
            item_id: None,
            name: name.to_string(),
            path: folders.join(" / "),
            method,
            url: Template::parse(&url_string),
            headers,
            body,
            auth,
            pre_scripts: ctx.pre.clone(),
            test_scripts: ctx.test.clone(),
        })
    }

    fn auth(&mut self, auth: Option<&RawAuth>, name: &str) -> AuthSpec {
        let Some(a) = auth else { return AuthSpec::None };
        let p = |key: &str| auth_param(&a.params, &a.kind, key).unwrap_or_default();
        match a.kind.as_str() {
            "noauth" => AuthSpec::None,
            "bearer" => AuthSpec::Bearer(Template::parse(&p("token"))),
            "basic" => AuthSpec::Basic { user: Template::parse(&p("username")), pass: Template::parse(&p("password")) },
            "apikey" => AuthSpec::ApiKey {
                key: Template::parse(&p("key")),
                value: Template::parse(&p("value")),
                in_query: p("in") == "query",
            },
            "oauth2" => {
                let token = p("accessToken");
                if token.is_empty() {
                    self.warn(format!("`{name}`: OAuth 2.0 without a stored access token is sent unauthenticated"));
                    AuthSpec::None
                } else {
                    AuthSpec::Bearer(Template::parse(&token))
                }
            }
            other => {
                self.warn(format!("`{name}`: auth type `{other}` is not supported yet; request is sent without it"));
                AuthSpec::None
            }
        }
    }

    fn body(&mut self, b: &RawBody, name: &str) -> Result<BodySpec> {
        Ok(match b.mode.as_deref() {
            Some("raw") => {
                let language = b.options.as_ref().and_then(|o| o.pointer("/raw/language")).and_then(Value::as_str);
                let content_type = match language {
                    Some("json") => Some("application/json"),
                    Some("xml") => Some("application/xml"),
                    Some("html") => Some("text/html"),
                    Some("javascript") => Some("application/javascript"),
                    _ => Some("text/plain"),
                };
                BodySpec::Raw { text: Template::parse(b.raw.as_deref().unwrap_or("")), content_type }
            }
            Some("urlencoded") => BodySpec::UrlEncoded(
                b.urlencoded
                    .iter()
                    .filter(|kv| !kv.disabled)
                    .filter_map(|kv| {
                        let k = kv.key.as_deref()?;
                        let v = kv.value.as_ref().map(json_to_string).unwrap_or_default();
                        Some((Template::parse(k), Template::parse(&v)))
                    })
                    .collect(),
            ),
            Some("formdata") => {
                let mut parts = Vec::new();
                for f in b.formdata.iter().filter(|f| !f.disabled) {
                    let Some(key) = f.key.as_deref() else { continue };
                    if f.kind.as_deref() == Some("file") {
                        let srcs: Vec<String> = match &f.src {
                            Some(Value::String(s)) => vec![s.clone()],
                            Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
                            _ => Vec::new(),
                        };
                        for src in srcs {
                            let data = self.read_file(&src).context("form-data body")?;
                            let file_name = Path::new(&src)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "file".into());
                            parts.push(FormPart::File {
                                name: Template::parse(key),
                                file_name,
                                data,
                                content_type: f.content_type.clone(),
                            });
                        }
                    } else {
                        let v = f.value.as_ref().map(json_to_string).unwrap_or_default();
                        parts.push(FormPart::Text {
                            name: Template::parse(key),
                            value: Template::parse(&v),
                            content_type: f.content_type.clone(),
                        });
                    }
                }
                BodySpec::Multipart(parts)
            }
            Some("file") => match b.file.as_ref().and_then(|f| f.src.as_deref()) {
                Some(src) => BodySpec::Bytes { data: self.read_file(src).context("binary body")?, content_type: None },
                None => BodySpec::None,
            },
            Some("graphql") => {
                let g = b.graphql.as_ref();
                BodySpec::Graphql {
                    query: Template::parse(g.and_then(|g| g.query.as_deref()).unwrap_or("")),
                    variables: Template::parse(g.and_then(|g| g.variables.as_deref()).unwrap_or("")),
                }
            }
            Some(other) => {
                self.warn(format!("`{name}`: body mode `{other}` is not supported; sending no body"));
                BodySpec::None
            }
            None => BodySpec::None,
        })
    }
}

/// v2.1 stores auth params as `[{key, value}]`, v2.0 as `{key: value}`.
fn auth_param(params: &HashMap<String, Value>, kind: &str, key: &str) -> Option<String> {
    match params.get(kind)? {
        Value::Array(list) => list
            .iter()
            .find(|p| p.get("key").and_then(Value::as_str) == Some(key))
            .and_then(|p| p.get("value"))
            .map(json_to_string),
        Value::Object(map) => map.get(key).map(json_to_string),
        _ => None,
    }
}

fn url_to_string(url: &RawUrl) -> String {
    let s = url_without_path_vars(url);
    match url {
        RawUrl::Obj(o) if !o.variable.is_empty() => apply_path_vars(&s, &o.variable),
        _ => s,
    }
}

/// Replace `:name` path segments with the values from `url.variable`.
fn apply_path_vars(url: &str, vars: &[RawKv]) -> String {
    let (path, rest) = match url.find(['?', '#']) {
        Some(i) => url.split_at(i),
        None => (url, ""),
    };
    let replaced: Vec<String> = path
        .split('/')
        .map(|seg| {
            seg.strip_prefix(':')
                .and_then(|name| vars.iter().find(|v| !v.disabled && v.key.as_deref() == Some(name)))
                .map(|v| v.value.as_ref().map(json_to_string).unwrap_or_default())
                .unwrap_or_else(|| seg.to_string())
        })
        .collect();
    replaced.join("/") + rest
}

fn url_without_path_vars(url: &RawUrl) -> String {
    match url {
        RawUrl::Raw(s) => s.clone(),
        RawUrl::Obj(o) => {
            if let Some(raw) = o.raw.as_ref().filter(|r| !r.is_empty()) {
                return raw.clone();
            }
            let mut s = String::new();
            if let Some(p) = &o.protocol {
                s.push_str(p);
                s.push_str("://");
            }
            match &o.host {
                Some(StrOrList::One(h)) => s.push_str(h),
                Some(StrOrList::Many(parts)) => s.push_str(&parts.join(".")),
                None => {}
            }
            if let Some(port) = &o.port {
                s.push(':');
                s.push_str(port);
            }
            match &o.path {
                Some(PathParts::One(p)) => {
                    if !p.starts_with('/') {
                        s.push('/');
                    }
                    s.push_str(p);
                }
                Some(PathParts::Many(parts)) => {
                    for part in parts {
                        s.push('/');
                        match part {
                            Value::String(p) => s.push_str(p),
                            Value::Object(m) => s.push_str(m.get("value").and_then(Value::as_str).unwrap_or("")),
                            _ => {}
                        }
                    }
                }
                None => {}
            }
            let query: Vec<String> = o
                .query
                .iter()
                .filter(|q| !q.disabled)
                .filter_map(|q| {
                    let k = q.key.as_deref()?;
                    Some(match &q.value {
                        Some(v) if !v.is_null() => format!("{k}={}", json_to_string(v)),
                        _ => k.to_string(),
                    })
                })
                .collect();
            if !query.is_empty() {
                s.push('?');
                s.push_str(&query.join("&"));
            }
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::NoVars;

    const SAMPLE: &str = r#"{
      "info": {"name": "Shop", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
      "auth": {"type": "bearer", "bearer": [{"key": "token", "value": "{{token}}", "type": "string"}]},
      "variable": [{"key": "base", "value": "http://localhost:8080"}, {"key": "n", "value": 3}],
      "event": [{"listen": "prerequest", "script": {"exec": ["pm.variables.set('t', Date.now())"]}}],
      "item": [
        {"name": "Auth", "auth": {"type": "noauth"}, "item": [
          {"name": "Login", "request": {"method": "post", "url": "{{base}}/login",
            "body": {"mode": "raw", "raw": "{\"u\":\"{{user}}\"}", "options": {"raw": {"language": "json"}}}},
            "event": [{"listen": "test", "script": {"exec": ["pm.test('ok', () => pm.response.to.have.status(200));"]}}]}
        ]},
        {"name": "Catalog", "item": [
          {"name": "List", "request": {"method": "GET", "header": [{"key": "Accept", "value": "application/json"}, {"key": "X-Off", "value": "1", "disabled": true}],
            "url": {"protocol": "http", "host": ["localhost"], "port": "8080", "path": ["items", ":id"], "query": [{"key": "page", "value": "1"}, {"key": "skip", "value": "x", "disabled": true}],
                    "variable": [{"key": "id", "value": "{{itemId}}"}]}}},
          {"name": "Inherit", "request": {"method": "GET", "auth": {"type": "inherit"}, "url": {"raw": "{{base}}/me"}}},
          {"name": "Form", "request": {"method": "POST", "url": "{{base}}/f", "body": {"mode": "urlencoded", "urlencoded": [{"key": "a", "value": "1"}, {"key": "b", "value": "2", "disabled": true}]}}}
        ]}
      ]
    }"#;

    fn plan() -> Plan {
        parse(SAMPLE, Path::new("."), &Selection::default()).unwrap()
    }

    #[test]
    fn flattens_in_order() {
        let p = plan();
        let names: Vec<_> = p.requests.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(names, ["Auth / Login", "Catalog / List", "Catalog / Inherit", "Catalog / Form"]);
        assert_eq!(p.requests[0].method, "POST");
        assert_eq!(p.variables["n"], "3");
    }

    #[test]
    fn url_object_without_raw() {
        let p = plan();
        assert_eq!(p.requests[1].url.render(&NoVars), "http://localhost:8080/items/{{itemId}}?page=1");
        assert_eq!(p.requests[1].headers.len(), 1);
    }

    #[test]
    fn auth_inheritance() {
        let p = plan();
        assert!(matches!(p.requests[0].auth, AuthSpec::None));
        assert!(matches!(p.requests[1].auth, AuthSpec::Bearer(_)));
        assert!(matches!(p.requests[2].auth, AuthSpec::Bearer(_)));
    }

    #[test]
    fn scripts_are_inherited_and_deduplicated() {
        let p = plan();
        assert_eq!(p.scripts.len(), 2);
        assert_eq!(p.requests[0].pre_scripts, vec![0]);
        assert_eq!(p.requests[0].test_scripts, vec![1]);
        assert_eq!(p.requests[3].pre_scripts, vec![0]);
        assert!(p.requests[3].test_scripts.is_empty());
    }

    #[test]
    fn bodies() {
        let p = plan();
        match &p.requests[0].body {
            BodySpec::Raw { content_type, .. } => assert_eq!(*content_type, Some("application/json")),
            other => panic!("{other:?}"),
        }
        match &p.requests[3].body {
            BodySpec::UrlEncoded(kv) => assert_eq!(kv.len(), 1),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn selection_filters() {
        let sel = Selection { folders: vec!["Catalog".into()], requests: vec![] };
        let p = parse(SAMPLE, Path::new("."), &sel).unwrap();
        assert_eq!(p.requests.len(), 3);
        let sel = Selection { folders: vec![], requests: vec!["Login".into()] };
        assert_eq!(parse(SAMPLE, Path::new("."), &sel).unwrap().requests.len(), 1);
    }
}
