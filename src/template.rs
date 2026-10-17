//! `{{variable}}` templates, pre-parsed once so the hot path only concatenates.
//!
//! Resolution follows Postman semantics: unknown variables are left untouched
//! (`{{missing}}` stays in the output), values that themselves contain
//! `{{...}}` are resolved recursively (bounded depth), and names starting with
//! `$` fall back to dynamic variables such as `{{$guid}}`.

use crate::dynamic;

/// Maximum nesting depth for variables whose values reference other variables.
const MAX_DEPTH: usize = 19;

/// Anything that can answer "what is the value of this variable?".
pub trait Lookup {
    fn lookup(&self, name: &str) -> Option<&str>;
}

impl Lookup for std::collections::HashMap<String, String> {
    fn lookup(&self, name: &str) -> Option<&str> {
        self.get(name).map(String::as_str)
    }
}

/// A lookup with no variables at all; dynamic variables still resolve.
pub struct NoVars;
impl Lookup for NoVars {
    fn lookup(&self, _: &str) -> Option<&str> {
        None
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Seg {
    Lit(String),
    Var(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    segs: Vec<Seg>,
    /// Upper bound of the literal length, used to pre-size output buffers.
    lit_len: usize,
}

impl Template {
    pub fn parse(src: &str) -> Template {
        let mut segs = Vec::new();
        let mut lit = String::new();
        let mut rest = src;
        while let Some((before, name, after)) = next_var(rest) {
            lit.push_str(before);
            if !lit.is_empty() {
                segs.push(Seg::Lit(std::mem::take(&mut lit)));
            }
            segs.push(Seg::Var(name.to_string()));
            rest = after;
        }
        lit.push_str(rest);
        if !lit.is_empty() {
            segs.push(Seg::Lit(lit));
        }
        let lit_len = segs
            .iter()
            .map(|s| match s {
                Seg::Lit(l) => l.len(),
                Seg::Var(_) => 16,
            })
            .sum();
        Template { segs, lit_len }
    }

    /// `Some(text)` when the template contains no variables.
    pub fn as_static(&self) -> Option<&str> {
        match self.segs.as_slice() {
            [] => Some(""),
            [Seg::Lit(l)] => Some(l),
            _ => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.segs.is_empty()
    }

    pub fn render(&self, vars: &dyn Lookup) -> String {
        if let Some(s) = self.as_static() {
            return s.to_string();
        }
        let mut out = String::with_capacity(self.lit_len);
        for seg in &self.segs {
            match seg {
                Seg::Lit(l) => out.push_str(l),
                Seg::Var(name) => push_var(name, vars, 0, &mut out),
            }
        }
        out
    }

    /// The template text as written, with `{{variables}}` left in place.
    pub fn source(&self) -> String {
        let mut s = String::with_capacity(self.lit_len);
        for seg in &self.segs {
            match seg {
                Seg::Lit(l) => s.push_str(l),
                Seg::Var(n) => {
                    s.push_str("{{");
                    s.push_str(n);
                    s.push_str("}}");
                }
            }
        }
        s
    }

    /// Names of the variables referenced directly by this template.
    pub fn variables(&self) -> impl Iterator<Item = &str> {
        self.segs.iter().filter_map(|s| match s {
            Seg::Var(n) => Some(n.as_str()),
            Seg::Lit(_) => None,
        })
    }
}

/// Resolve every `{{var}}` inside an arbitrary string (used for nested values).
pub fn resolve_str(src: &str, vars: &dyn Lookup) -> String {
    let mut out = String::with_capacity(src.len());
    resolve_into(src, vars, 0, &mut out);
    out
}

fn resolve_into(src: &str, vars: &dyn Lookup, depth: usize, out: &mut String) {
    let mut rest = src;
    while let Some((before, name, after)) = next_var(rest) {
        out.push_str(before);
        push_var(name, vars, depth, out);
        rest = after;
    }
    out.push_str(rest);
}

fn push_var(name: &str, vars: &dyn Lookup, depth: usize, out: &mut String) {
    if let Some(v) = vars.lookup(name) {
        if depth < MAX_DEPTH && v.contains("{{") {
            resolve_into(v, vars, depth + 1, out);
        } else {
            out.push_str(v);
        }
    } else if let Some(v) = name.strip_prefix('$').and_then(dynamic::generate) {
        out.push_str(&v);
    } else {
        out.push_str("{{");
        out.push_str(name);
        out.push_str("}}");
    }
}

/// Find the next `{{name}}` where `name` contains no braces.
/// Returns (text before, name, text after).
fn next_var(s: &str) -> Option<(&str, &str, &str)> {
    let mut from = 0;
    while let Some(open) = s[from..].find("{{").map(|i| i + from) {
        let inner_start = open + 2;
        let tail = &s[inner_start..];
        match tail.find(['{', '}']) {
            Some(i) if tail[i..].starts_with("}}") && i > 0 => {
                let name = &tail[..i];
                return Some((&s[..open], name, &tail[i + 2..]));
            }
            // `{{{x}}` – skip one brace and retry so the inner `{{x}}` matches.
            Some(_) => from = open + 1,
            None => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn renders_literals_and_vars() {
        let t = Template::parse("{{base}}/users/{{id}}?q=1");
        let v = vars(&[("base", "http://x"), ("id", "42")]);
        assert_eq!(t.render(&v), "http://x/users/42?q=1");
        assert!(t.as_static().is_none());
        assert_eq!(Template::parse("plain").as_static(), Some("plain"));
    }

    #[test]
    fn unknown_variables_are_kept() {
        let t = Template::parse("a{{nope}}b");
        assert_eq!(t.render(&NoVars), "a{{nope}}b");
    }

    #[test]
    fn nested_values_resolve() {
        let v = vars(&[("host", "{{scheme}}://api"), ("scheme", "https")]);
        assert_eq!(Template::parse("{{host}}/v1").render(&v), "https://api/v1");
    }

    #[test]
    fn self_reference_terminates() {
        let v = vars(&[("a", "{{a}}")]);
        let out = Template::parse("{{a}}").render(&v);
        assert_eq!(out, "{{a}}");
    }

    #[test]
    fn triple_braces_and_empty_names() {
        let v = vars(&[("x", "1")]);
        assert_eq!(Template::parse("{{{x}}}").render(&v), "{1}");
        assert_eq!(Template::parse("{{}}").render(&v), "{{}}");
    }

    #[test]
    fn dynamic_variables() {
        let out = Template::parse("{{$guid}}").render(&NoVars);
        assert_eq!(out.len(), 36);
        let ts: u64 = Template::parse("{{$timestamp}}").render(&NoVars).parse().unwrap();
        assert!(ts > 1_600_000_000);
        // A defined variable shadows the dynamic one.
        let v = vars(&[("$guid", "fixed")]);
        assert_eq!(Template::parse("{{$guid}}").render(&v), "fixed");
    }
}
