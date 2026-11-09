//! The result file: one JSON document per run, and the min/median/max across
//! repeated runs of a scenario.

use serde::{Deserialize, Serialize};

pub const SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Fail,
    /// The host broke the run (a media write error); it says nothing about
    /// the emulator and is never counted as a pass or a failure.
    Invalid,
    /// An asset the scenario needs is absent.
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PhaseResult {
    pub name: String,
    /// Seconds from CPU start to the match.
    pub at_secs: f64,
    /// Seconds since the previous phase.
    pub delta_secs: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StepResult {
    pub name: String,
    pub passed: bool,
    pub secs: f64,
    /// The first 2000 bytes of what the step printed, for reading a result
    /// without re-running it.
    #[serde(default)]
    pub output: String,
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ForbidResult {
    pub pattern: String,
    pub count: u64,
    pub max: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Host {
    pub load1: f64,
    pub cpus: usize,
    pub free_gb: u64,
    pub timeout_scale: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunResult {
    pub schema: u32,
    pub scenario: String,
    pub status: Status,
    pub started_unix: u64,
    /// Free text from `--label`, e.g. the commit under test.
    #[serde(default)]
    pub label: String,
    /// BLAKE3 of the iris binary, so numbers are tied to the build.
    pub iris_blake3: String,
    /// Cargo features of the iris build (`jitv2` is how the JIT is told from
    /// the interpreter), from the line iris prints at startup.
    #[serde(default)]
    pub build_features: Vec<String>,
    pub host: Host,
    pub phases: Vec<PhaseResult>,
    pub steps: Vec<StepResult>,
    pub forbid: Vec<ForbidResult>,
    /// None when the scenario has no `[finish]`.
    pub exited_by_itself: Option<bool>,
    pub total_secs: f64,
    /// Why the run did not pass.
    pub problems: Vec<String>,
}

impl RunResult {
    /// Seconds at which the named phase matched.
    pub fn phase_at(&self, name: &str) -> Option<f64> {
        self.phases.iter().find(|p| p.name == name).map(|p| p.at_secs)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Spread {
    pub min: f64,
    pub median: f64,
    pub max: f64,
}

pub fn spread(values: &[f64]) -> Option<Spread> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let mid = v.len() / 2;
    let median = if v.len() % 2 == 1 { v[mid] } else { (v[mid - 1] + v[mid]) / 2.0 };
    Some(Spread { min: v[0], median, max: v[v.len() - 1] })
}

/// Each phase's arrival time across the passing runs, in the first run's
/// phase order. Invalid and skipped runs are left out: their numbers are not
/// the emulator's.
pub fn summarize(runs: &[RunResult]) -> Vec<(String, Spread)> {
    let good: Vec<&RunResult> = runs.iter().filter(|r| r.status == Status::Pass).collect();
    let Some(first) = good.first() else { return Vec::new() };
    let mut out: Vec<(String, Spread)> = first
        .phases
        .iter()
        .filter_map(|p| {
            let at: Vec<f64> = good.iter().filter_map(|r| r.phase_at(&p.name)).collect();
            spread(&at).map(|s| (p.name.clone(), s))
        })
        .collect();
    let totals: Vec<f64> = good.iter().map(|r| r.total_secs).collect();
    out.extend(spread(&totals).map(|s| ("total".to_string(), s)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(status: Status, login: f64, total: f64) -> RunResult {
        RunResult {
            schema: SCHEMA,
            scenario: "s".into(),
            status,
            started_unix: 1,
            label: "v".into(),
            build_features: vec![],
            iris_blake3: "h".into(),
            host: Host { load1: 1.0, cpus: 4, free_gb: 200, timeout_scale: 1.0 },
            phases: vec![PhaseResult { name: "login".into(), at_secs: login, delta_secs: login }],
            steps: vec![],
            forbid: vec![],
            exited_by_itself: Some(true),
            total_secs: total,
            problems: vec![],
        }
    }

    #[test]
    fn spread_handles_odd_even_and_empty_inputs() {
        assert_eq!(spread(&[]), None);
        assert_eq!(spread(&[3.0, 1.0, 2.0]), Some(Spread { min: 1.0, median: 2.0, max: 3.0 }));
        assert_eq!(spread(&[4.0, 1.0]), Some(Spread { min: 1.0, median: 2.5, max: 4.0 }));
    }

    #[test]
    fn only_passing_runs_feed_the_summary() {
        let runs = [
            run(Status::Pass, 180.0, 200.0),
            run(Status::Invalid, 999.0, 999.0),
            run(Status::Pass, 170.0, 190.0),
            run(Status::Fail, 5.0, 5.0),
        ];
        let s = summarize(&runs);
        assert_eq!(s[0], ("login".into(), Spread { min: 170.0, median: 175.0, max: 180.0 }));
        assert_eq!(s[1].0, "total");
    }

    #[test]
    fn a_result_survives_a_json_round_trip() {
        let r = run(Status::Pass, 1.5, 2.5);
        let text = serde_json::to_string(&r).unwrap();
        assert!(text.contains("\"status\":\"pass\""));
        assert_eq!(serde_json::from_str::<RunResult>(&text).unwrap(), r);
    }
}
