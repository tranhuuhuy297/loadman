//! Iteration data files (CSV or JSON array of objects) and how rows are
//! handed out to virtual users.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::vars::{Row, json_to_string};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Distribution {
    /// VU n gets row n; VUs beyond the last row run without data.
    Ordered,
    /// A random row every time the VU starts the requests again.
    Random,
    /// VU n gets row n modulo the row count.
    Sequential,
    /// Rows are handed out in turn as VUs start (a shared counter).
    RoundRobin,
}

#[derive(Debug)]
pub struct DataSet {
    pub rows: Vec<Row>,
    pub distribution: Distribution,
    /// When set, a new row is picked at the start of every iteration instead
    /// of once per virtual user (`Random` always does this).
    pub per_iteration: bool,
}

impl DataSet {
    pub fn load(path: &Path, distribution: Distribution, per_iteration: bool) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("cannot read data file {}", path.display()))?;
        let is_json =
            path.extension().is_some_and(|e| e.eq_ignore_ascii_case("json")) || text.trim_start().starts_with('[');
        let rows = if is_json { parse_json(&text)? } else { parse_csv(&text)? };
        if rows.is_empty() {
            bail!("data file {} has no rows", path.display());
        }
        Ok(DataSet { rows, distribution, per_iteration })
    }

    /// Row for a virtual user. `vu` is the VU index; `counter` is a value
    /// that increases on every pick across all VUs (used by round-robin and,
    /// in per-iteration mode, by sequential distribution).
    pub fn pick(&self, vu: usize, iteration: u64, counter: u64) -> Option<Row> {
        let n = self.rows.len();
        let idx = match (self.distribution, self.per_iteration) {
            (Distribution::Random, _) => fastrand::usize(..n),
            (Distribution::RoundRobin, _) => (counter % n as u64) as usize,
            (Distribution::Sequential, false) => vu % n,
            (Distribution::Sequential, true) => (counter % n as u64) as usize,
            (Distribution::Ordered, false) => vu,
            (Distribution::Ordered, true) => iteration as usize,
        };
        self.rows.get(idx).cloned()
    }
}

fn parse_csv(text: &str) -> Result<Vec<Row>> {
    let mut rdr = csv::ReaderBuilder::new().trim(csv::Trim::Headers).flexible(true).from_reader(text.as_bytes());
    let headers = rdr.headers().context("CSV data file has no header row")?.clone();
    let mut rows = Vec::new();
    for rec in rdr.records() {
        let rec = rec.context("invalid CSV row")?;
        let row: HashMap<String, String> =
            headers.iter().zip(rec.iter()).map(|(k, v)| (k.to_string(), v.to_string())).collect();
        rows.push(Arc::new(row));
    }
    Ok(rows)
}

fn parse_json(text: &str) -> Result<Vec<Row>> {
    let v: Value = serde_json::from_str(text).context("JSON data file is not valid JSON")?;
    let Value::Array(items) = v else { bail!("JSON data file must be an array of objects") };
    items
        .into_iter()
        .map(|item| match item {
            Value::Object(map) => Ok(Arc::new(map.iter().map(|(k, v)| (k.clone(), json_to_string(v))).collect())),
            _ => bail!("every element of the JSON data file must be an object"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(dist: Distribution, per_iteration: bool) -> DataSet {
        DataSet { rows: parse_csv("user,pass\na,1\nb,2\nc,3\n").unwrap(), distribution: dist, per_iteration }
    }

    fn user(r: Option<Row>) -> Option<String> {
        r.map(|r| r["user"].clone())
    }

    #[test]
    fn csv_and_json() {
        assert_eq!(parse_csv("a, b\n1,2\n").unwrap()[0]["b"], "2");
        let j = parse_json(r#"[{"id": 1, "name": "x"}]"#).unwrap();
        assert_eq!(j[0]["id"], "1");
        assert!(parse_json(r#"{"a":1}"#).is_err());
    }

    #[test]
    fn distributions() {
        let d = set(Distribution::Ordered, false);
        assert_eq!(user(d.pick(1, 0, 0)), Some("b".into()));
        assert_eq!(user(d.pick(5, 0, 0)), None);
        let d = set(Distribution::Sequential, false);
        assert_eq!(user(d.pick(4, 0, 0)), Some("b".into()));
        let d = set(Distribution::RoundRobin, false);
        assert_eq!(user(d.pick(0, 0, 5)), Some("c".into()));
        let d = set(Distribution::Ordered, true);
        assert_eq!(user(d.pick(9, 2, 0)), Some("c".into()));
        let d = set(Distribution::Random, false);
        assert!(d.pick(0, 0, 0).is_some());
    }
}
