//! Variable scopes. Precedence (highest first): local, iteration data,
//! environment, collection, globals — the same order Postman documents.
//!
//! The initial values are shared by every virtual user through an `Arc`; each
//! VU only stores the changes its scripts make, so starting thousands of VUs
//! does not copy the environment thousands of times.

use std::collections::HashMap;
use std::sync::Arc;

use crate::template::Lookup;

pub type Row = Arc<HashMap<String, String>>;

#[derive(Debug, Default, Clone)]
pub struct BaseVars {
    pub globals: HashMap<String, String>,
    pub collection: HashMap<String, String>,
    pub environment: HashMap<String, String>,
    pub environment_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Collection,
    Environment,
    Local,
}

impl Scope {
    pub fn from_code(c: &str) -> Option<Scope> {
        Some(match c {
            "g" => Scope::Global,
            "c" => Scope::Collection,
            "e" => Scope::Environment,
            "l" => Scope::Local,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum VarOp {
    Set(Scope, String, String),
    Unset(Scope, String),
    Clear(Scope),
}

#[derive(Debug, Default)]
struct Overlay {
    /// `None` marks an unset key that hides the shared value.
    map: HashMap<String, Option<String>>,
    cleared: bool,
}

impl Overlay {
    fn get<'a>(&'a self, base: &'a HashMap<String, String>, k: &str) -> Option<&'a str> {
        match self.map.get(k) {
            Some(Some(v)) => Some(v),
            Some(None) => None,
            None if self.cleared => None,
            None => base.get(k).map(String::as_str),
        }
    }
}

/// Variable state owned by one virtual user.
pub struct VuVars {
    base: Arc<BaseVars>,
    globals: Overlay,
    collection: Overlay,
    environment: Overlay,
    local: HashMap<String, String>,
    data: Option<Row>,
    /// Command-line `--env-var` overrides; they win over the environment file.
    overrides: Arc<HashMap<String, String>>,
}

impl VuVars {
    pub fn new(base: Arc<BaseVars>, overrides: Arc<HashMap<String, String>>, data: Option<Row>) -> Self {
        VuVars {
            base,
            globals: Overlay::default(),
            collection: Overlay::default(),
            environment: Overlay::default(),
            local: HashMap::new(),
            data,
            overrides,
        }
    }

    pub fn set_data(&mut self, data: Option<Row>) {
        self.data = data;
    }

    /// Local variables only live for one iteration.
    pub fn reset_local(&mut self) {
        self.local.clear();
    }

    fn overlay(&mut self, scope: Scope) -> &mut Overlay {
        match scope {
            Scope::Global => &mut self.globals,
            Scope::Collection => &mut self.collection,
            Scope::Environment | Scope::Local => &mut self.environment,
        }
    }

    pub fn apply(&mut self, op: VarOp) {
        match op {
            VarOp::Set(Scope::Local, k, v) => {
                self.local.insert(k, v);
            }
            VarOp::Unset(Scope::Local, k) => {
                self.local.remove(&k);
            }
            VarOp::Clear(Scope::Local) => self.local.clear(),
            VarOp::Set(scope, k, v) => {
                self.overlay(scope).map.insert(k, Some(v));
            }
            VarOp::Unset(scope, k) => {
                self.overlay(scope).map.insert(k, None);
            }
            VarOp::Clear(scope) => {
                let o = self.overlay(scope);
                o.map.clear();
                o.cleared = true;
            }
        }
    }
}

impl Lookup for VuVars {
    fn lookup(&self, name: &str) -> Option<&str> {
        if let Some(v) = self.local.get(name) {
            return Some(v);
        }
        if let Some(v) = self.data.as_ref().and_then(|d| d.get(name)) {
            return Some(v);
        }
        if let Some(v) = self.environment.map.get(name) {
            if let Some(v) = v {
                return Some(v);
            }
        } else if let Some(v) = self.overrides.get(name) {
            return Some(v);
        }
        self.environment
            .get(&self.base.environment, name)
            .or_else(|| self.collection.get(&self.base.collection, name))
            .or_else(|| self.globals.get(&self.base.globals, name))
    }
}

/// Convert a JSON value from a Postman file into the string used for substitution.
pub fn json_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Arc<BaseVars> {
        let mut b = BaseVars::default();
        b.globals.insert("g".into(), "global".into());
        b.globals.insert("shared".into(), "from-global".into());
        b.collection.insert("shared".into(), "from-collection".into());
        b.environment.insert("host".into(), "env-host".into());
        Arc::new(b)
    }

    #[test]
    fn precedence() {
        let mut row = HashMap::new();
        row.insert("host".to_string(), "data-host".to_string());
        let mut v = VuVars::new(base(), Arc::default(), Some(Arc::new(row)));
        assert_eq!(v.lookup("host"), Some("data-host"));
        assert_eq!(v.lookup("shared"), Some("from-collection"));
        assert_eq!(v.lookup("g"), Some("global"));
        v.apply(VarOp::Set(Scope::Local, "host".into(), "local-host".into()));
        assert_eq!(v.lookup("host"), Some("local-host"));
        v.reset_local();
        v.set_data(None);
        assert_eq!(v.lookup("host"), Some("env-host"));
    }

    #[test]
    fn unset_and_clear_hide_shared_values() {
        let mut v = VuVars::new(base(), Arc::default(), None);
        v.apply(VarOp::Unset(Scope::Collection, "shared".into()));
        assert_eq!(v.lookup("shared"), Some("from-global"));
        v.apply(VarOp::Clear(Scope::Global));
        assert_eq!(v.lookup("shared"), None);
        v.apply(VarOp::Set(Scope::Global, "shared".into(), "new".into()));
        assert_eq!(v.lookup("shared"), Some("new"));
    }

    #[test]
    fn overrides_beat_environment_file_but_not_scripts() {
        let mut o = HashMap::new();
        o.insert("host".to_string(), "cli-host".to_string());
        let mut v = VuVars::new(base(), Arc::new(o), None);
        assert_eq!(v.lookup("host"), Some("cli-host"));
        v.apply(VarOp::Set(Scope::Environment, "host".into(), "script-host".into()));
        assert_eq!(v.lookup("host"), Some("script-host"));
    }
}
