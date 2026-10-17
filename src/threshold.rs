//! Pass/fail criteria for CI: `--threshold "p95<300ms"`, `"error_rate<1%"`.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::report::Totals;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub enum Op {
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, Serialize)]
pub struct Threshold {
    pub expr: String,
    pub metric: String,
    pub op: Op,
    pub value: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThresholdResult {
    pub expr: String,
    pub actual: f64,
    pub unit: &'static str,
    pub passed: bool,
}

impl ThresholdResult {
    pub fn actual_text(&self) -> String {
        match self.unit {
            "ms" => crate::report::ms(self.actual),
            "%" => format!("{:.2}%", self.actual),
            "req/s" => format!("{:.1} req/s", self.actual),
            _ => format!("{}", self.actual),
        }
    }
}

const METRICS: &[&str] = &[
    "avg",
    "min",
    "max",
    "p50",
    "med",
    "p90",
    "p95",
    "p99",
    "rps",
    "throughput",
    "error_rate",
    "http_error_rate",
    "assertion_failure_rate",
    "requests",
];

impl Threshold {
    pub fn parse(expr: &str) -> Result<Threshold> {
        let s: String = expr.chars().filter(|c| !c.is_whitespace()).collect();
        let (pos, op, len) = [("<=", Op::Le), (">=", Op::Ge), ("<", Op::Lt), (">", Op::Gt)]
            .iter()
            .find_map(|(tok, op)| s.find(tok).map(|i| (i, *op, tok.len())))
            .ok_or_else(|| anyhow::anyhow!("threshold `{expr}` needs <, <=, > or >= (e.g. p95<300ms)"))?;
        let metric = s[..pos].to_lowercase();
        if !METRICS.contains(&metric.as_str()) {
            bail!("unknown threshold metric `{metric}`; use one of {}", METRICS.join(", "));
        }
        let raw = &s[pos + len..];
        let (num, factor) = if let Some(n) = raw.strip_suffix("ms") {
            (n, 1.0)
        } else if let Some(n) = raw.strip_suffix('s') {
            (n, 1000.0)
        } else if let Some(n) = raw.strip_suffix('%') {
            (n, 1.0)
        } else {
            (raw, 1.0)
        };
        let value: f64 = num.parse().map_err(|_| anyhow::anyhow!("threshold `{expr}`: `{raw}` is not a number"))?;
        Ok(Threshold { expr: expr.trim().to_string(), metric, op, value: value * factor })
    }

    pub fn evaluate(&self, t: &Totals) -> ThresholdResult {
        let actual = match self.metric.as_str() {
            "avg" => t.avg_ms,
            "min" => t.min_ms,
            "max" => t.max_ms,
            "p50" | "med" => t.p50_ms,
            "p90" => t.p90_ms,
            "p95" => t.p95_ms,
            "p99" => t.p99_ms,
            "rps" | "throughput" => t.rps,
            "error_rate" => t.error_rate,
            "http_error_rate" => t.http_error_rate,
            "assertion_failure_rate" => t.assertion_failure_rate,
            "requests" => t.requests as f64,
            _ => f64::NAN,
        };
        let passed = match self.op {
            Op::Lt => actual < self.value,
            Op::Le => actual <= self.value,
            Op::Gt => actual > self.value,
            Op::Ge => actual >= self.value,
        };
        let unit = match self.metric.as_str() {
            "rps" | "throughput" => "req/s",
            "error_rate" | "http_error_rate" | "assertion_failure_rate" => "%",
            "requests" => "",
            _ => "ms",
        };
        ThresholdResult { expr: self.expr.clone(), actual, unit, passed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        let t = Threshold::parse("p95 < 0.3s").unwrap();
        assert_eq!((t.metric.as_str(), t.op, t.value), ("p95", Op::Lt, 300.0));
        let t = Threshold::parse("error_rate<=1%").unwrap();
        assert_eq!((t.op, t.value), (Op::Le, 1.0));
        assert_eq!(Threshold::parse("rps>=1500").unwrap().op, Op::Ge);
        assert!(Threshold::parse("latency<1").is_err());
        assert!(Threshold::parse("p95").is_err());
    }

    #[test]
    fn evaluates() {
        let totals = Totals { p95_ms: 250.0, error_rate: 2.0, ..Totals::default() };
        assert!(Threshold::parse("p95<300ms").unwrap().evaluate(&totals).passed);
        assert!(!Threshold::parse("error_rate<1%").unwrap().evaluate(&totals).passed);
    }
}
