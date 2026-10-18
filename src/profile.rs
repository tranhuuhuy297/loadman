//! Load profiles: how many virtual users should be active at each moment.
//!
//! A profile is a list of control points (time, VUs). Between two points the
//! VU count changes linearly and is rounded to a whole user; after the last
//! point it stays constant. The four named shapes and their default
//! proportions are described in docs/COMPATIBILITY.md.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileKind {
    /// Constant number of VUs for the whole run.
    Fixed,
    /// Hold a base load, ramp linearly to the maximum, then hold it.
    RampUp,
    /// Short burst from a base load to the maximum and back.
    Spike,
    /// Ramp up, hold the maximum, ramp back down to the base load.
    Peak,
    /// Control points supplied with `--stages` or `--load-profile`.
    Custom,
}

impl ProfileKind {
    pub fn label(self) -> &'static str {
        match self {
            ProfileKind::Fixed => "fixed",
            ProfileKind::RampUp => "ramp-up",
            ProfileKind::Spike => "spike",
            ProfileKind::Peak => "peak",
            ProfileKind::Custom => "custom",
        }
    }

    /// Default base load as a fraction of the maximum, and the shape as
    /// (time fraction, load fraction) pairs where `None` means "base load".
    fn shape(self) -> (f64, &'static [(f64, Option<f64>)]) {
        match self {
            ProfileKind::Fixed => (1.0, &[(0.0, Some(1.0)), (1.0, Some(1.0))]),
            ProfileKind::RampUp => (0.25, &[(0.0, None), (0.25, None), (0.5, Some(1.0)), (1.0, Some(1.0))]),
            ProfileKind::Spike => (0.1, &[(0.0, None), (0.4, None), (0.5, Some(1.0)), (0.6, None), (1.0, None)]),
            ProfileKind::Peak => {
                (0.2, &[(0.0, None), (0.2, None), (0.4, Some(1.0)), (0.6, Some(1.0)), (0.8, None), (1.0, None)])
            }
            ProfileKind::Custom => (1.0, &[]),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlPoint {
    pub time_ms: u64,
    pub vus: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadProfile {
    pub kind: ProfileKind,
    pub duration_ms: u64,
    pub points: Vec<ControlPoint>,
}

impl LoadProfile {
    /// Build one of the named shapes. `base_vus` overrides the default base load.
    pub fn named(kind: ProfileKind, max_vus: u32, duration: Duration, base_vus: Option<u32>) -> Result<Self> {
        if kind == ProfileKind::Custom {
            bail!("custom profiles need --stages or --load-profile");
        }
        if max_vus == 0 {
            bail!("--vus must be at least 1");
        }
        let duration_ms = duration.as_millis() as u64;
        if duration_ms == 0 {
            bail!("--duration must be greater than zero");
        }
        let (base_frac, shape) = kind.shape();
        let base = base_vus.unwrap_or_else(|| ((max_vus as f64 * base_frac).round() as u32).max(1)).min(max_vus);
        let points = shape
            .iter()
            .map(|&(t, load)| ControlPoint {
                time_ms: (duration_ms as f64 * t).round() as u64,
                vus: match load {
                    Some(f) => (max_vus as f64 * f).round() as u32,
                    None => base,
                },
            })
            .collect();
        LoadProfile { kind, duration_ms, points }.validated()
    }

    /// Parse `--stages "0s:5,30s:20,2m:20"`: absolute time offsets and VU counts.
    pub fn from_stages(spec: &str) -> Result<Self> {
        let mut points = Vec::new();
        for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (t, v) = part.split_once(':').with_context(|| format!("stage `{part}` must look like `30s:20`"))?;
            let time_ms = if t.trim() == "0" { 0 } else { parse_duration(t.trim())?.as_millis() as u64 };
            let vus = v.trim().parse().with_context(|| format!("stage `{part}`: VU count is not a number"))?;
            points.push(ControlPoint { time_ms, vus });
        }
        let duration_ms = points.iter().map(|p| p.time_ms).max().unwrap_or(0);
        if duration_ms == 0 {
            bail!("--stages needs at least one point after 0s");
        }
        LoadProfile { kind: ProfileKind::Custom, duration_ms, points }.validated()
    }

    /// Read a load profile exported in the Postman shape:
    /// `{type, options: {totalDuration: {millis}}, controlPoints: [{id, load: {count}, time: {millis}}]}`.
    pub fn from_postman_json(v: &Value) -> Result<Self> {
        let duration_ms = v
            .pointer("/options/totalDuration/millis")
            .and_then(Value::as_f64)
            .context("load profile is missing options.totalDuration.millis")? as u64;
        let list = v.get("controlPoints").and_then(Value::as_array).context("load profile has no controlPoints")?;
        let mut points = Vec::new();
        for cp in list {
            let time_ms =
                cp.pointer("/time/millis").and_then(Value::as_f64).context("control point without time.millis")?;
            let vus = cp.pointer("/load/count").and_then(Value::as_f64).context("control point without load.count")?;
            points.push(ControlPoint { time_ms: time_ms as u64, vus: vus.round() as u32 });
        }
        let kind = match v.get("type").and_then(Value::as_str) {
            Some("fixed") => ProfileKind::Fixed,
            Some("ramp-up") => ProfileKind::RampUp,
            Some("spike") => ProfileKind::Spike,
            Some("peak") => ProfileKind::Peak,
            _ => ProfileKind::Custom,
        };
        LoadProfile { kind, duration_ms, points }.validated()
    }

    fn validated(mut self) -> Result<Self> {
        if self.points.is_empty() {
            bail!("a load profile needs at least one control point");
        }
        self.points.sort_by_key(|p| p.time_ms);
        if self.points.iter().any(|p| p.time_ms > self.duration_ms) {
            bail!("control points must not be later than the total duration");
        }
        if self.max_vus() == 0 {
            bail!("a load profile needs at least one virtual user");
        }
        Ok(self)
    }

    pub fn duration(&self) -> Duration {
        Duration::from_millis(self.duration_ms)
    }

    pub fn max_vus(&self) -> u32 {
        self.points.iter().map(|p| p.vus).max().unwrap_or(0)
    }

    /// Target VU count `elapsed_ms` into the run: linear interpolation between
    /// neighbouring control points, rounded half up.
    pub fn target_vus(&self, elapsed_ms: u64) -> u32 {
        (self.load_at(elapsed_ms as f64) + 0.5).floor() as u32
    }

    /// The profile's value `elapsed_ms` into the run, before rounding.
    pub fn load_at(&self, elapsed_ms: f64) -> f64 {
        let pts = &self.points;
        let Some(first) = pts.first() else { return 0.0 };
        let last = pts[pts.len() - 1];
        if elapsed_ms >= self.duration_ms as f64 {
            return last.vus as f64;
        }
        if elapsed_ms <= first.time_ms as f64 {
            return first.vus as f64;
        }
        for w in pts.windows(2) {
            let (a, b) = (w[0], w[1]);
            if elapsed_ms <= b.time_ms as f64 {
                if b.time_ms == a.time_ms {
                    return b.vus as f64;
                }
                let f = (elapsed_ms - a.time_ms as f64) / (b.time_ms - a.time_ms) as f64;
                return a.vus as f64 + f * (b.vus as f64 - a.vus as f64);
            }
        }
        last.vus as f64
    }

    /// Area under the VU curve, in VU-hours (what cloud runners bill for).
    pub fn vu_hours(&self) -> f64 {
        let mut ms = 0.0;
        for w in self.points.windows(2) {
            let dt = (w[1].time_ms - w[0].time_ms) as f64;
            ms += dt * (w[0].vus as f64 + w[1].vus as f64) / 2.0;
        }
        if let Some(last) = self.points.last() {
            ms += (self.duration_ms - last.time_ms) as f64 * last.vus as f64;
        }
        ms / 3_600_000.0
    }

    /// The share of agent `i` of `n` in a distributed run: VU `k` of the
    /// whole run goes to agent `k % n`, so the shares add up to the total at
    /// every control point.
    pub fn share(&self, i: usize, n: usize) -> Result<Self> {
        let (i, n) = (i as u32, n as u32);
        let points = self.points.iter().map(|p| ControlPoint { vus: (p.vus + n - 1 - i) / n, ..*p }).collect();
        let share = LoadProfile { points, ..self.clone() };
        if share.max_vus() == 0 {
            bail!("{n} agents need at least {n} VUs at the profile's peak");
        }
        Ok(share)
    }

    /// One-line ASCII sparkline of the profile, for `loadman profile`.
    pub fn sparkline(&self, width: usize) -> String {
        const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let max = self.max_vus().max(1) as f64;
        (0..width)
            .map(|i| {
                let t = (self.duration_ms as f64 * i as f64 / (width.max(2) - 1) as f64) as u64;
                let v = self.target_vus(t) as f64 / max;
                BARS[((v * 7.0).round() as usize).min(7)]
            })
            .collect()
    }
}

/// `500ms`, `30s`, `2m`, `1h30m`; a bare number means seconds.
pub fn parse_duration(s: &str) -> Result<Duration> {
    if let Ok(secs) = s.parse::<f64>() {
        return Ok(Duration::from_secs_f64(secs));
    }
    humantime::parse_duration(s).with_context(|| format!("`{s}` is not a duration (try 30s, 5m, 1h)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn fixed_is_constant() {
        let p = LoadProfile::named(ProfileKind::Fixed, 20, secs(60), None).unwrap();
        assert_eq!(p.target_vus(0), 20);
        assert_eq!(p.target_vus(59_000), 20);
        assert!((p.vu_hours() - 20.0 / 60.0).abs() < 1e-9);
    }

    #[test]
    fn ramp_up_default_shape() {
        // 20 VUs over 60s: 5 VUs until 15s, ramp to 20 by 30s, hold.
        let p = LoadProfile::named(ProfileKind::RampUp, 20, secs(60), None).unwrap();
        assert_eq!(
            p.points.iter().map(|c| (c.time_ms, c.vus)).collect::<Vec<_>>(),
            vec![(0, 5), (15_000, 5), (30_000, 20), (60_000, 20)]
        );
        assert_eq!(p.target_vus(10_000), 5);
        assert_eq!(p.target_vus(22_500), 13); // 12.5 rounds half up
        assert_eq!(p.target_vus(45_000), 20);
    }

    #[test]
    fn spike_and_peak_shapes() {
        let s = LoadProfile::named(ProfileKind::Spike, 200, secs(100), None).unwrap();
        assert_eq!(s.target_vus(0), 20);
        assert_eq!(s.target_vus(50_000), 200);
        assert_eq!(s.target_vus(70_000), 20);
        let p = LoadProfile::named(ProfileKind::Peak, 20, secs(100), Some(2)).unwrap();
        assert_eq!(p.target_vus(10_000), 2);
        assert_eq!(p.target_vus(50_000), 20);
        assert_eq!(p.target_vus(90_000), 2);
    }

    #[test]
    fn stages_and_postman_json() {
        let p = LoadProfile::from_stages("0:1, 10s:11, 20s:11").unwrap();
        assert_eq!(p.duration_ms, 20_000);
        assert_eq!(p.target_vus(5_000), 6);
        let json: Value = serde_json::from_str(
            r#"{"type":"ramp-up","options":{"totalDuration":{"millis":20000}},
                "controlPoints":[{"id":"initial","load":{"count":5},"time":{"millis":0}},
                                 {"id":"rampLeadStart","load":{"count":5},"time":{"millis":5000}},
                                 {"id":"rampLeadEnd","load":{"count":20},"time":{"millis":10000}},
                                 {"id":"final","load":{"count":20},"time":{"millis":20000}}]}"#,
        )
        .unwrap();
        let p = LoadProfile::from_postman_json(&json).unwrap();
        assert_eq!(p.kind, ProfileKind::RampUp);
        assert_eq!(p.target_vus(7_500), 13);
        assert_eq!(p.max_vus(), 20);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(LoadProfile::named(ProfileKind::Fixed, 0, secs(10), None).is_err());
        assert!(LoadProfile::from_stages("0:0").is_err());
        assert!(LoadProfile::from_stages("10s").is_err());
    }
}
