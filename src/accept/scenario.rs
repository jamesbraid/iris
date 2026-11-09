//! A scenario file: one machine, how to boot it, and what to check once it is up.
//!
//! The harness knows nothing about any machine. Everything machine-specific
//! (profile, CPU, banks, the strings its console prints) is in the scenario's
//! `[iris]` table and `[[phase]]` list. See `test/accept/` for examples and
//! TESTING.md for the schema.

use std::collections::BTreeSet;

use serde::Deserialize;

/// Keys the harness writes itself. A scenario that sets one would fight the
/// harness over ports, log paths or which disk is booted.
const HARNESS_KEYS: &[&str] = &[
    "prom", "nvram", "headless", "no_audio", "monitor_port", "serial_port_a",
    "serial_port_b", "serial_log", "ci", "ci_socket", "scsi",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Speed {
    /// Minutes: suitable for every pull request.
    Quick,
    /// Tens of minutes or hours: nightly or by hand.
    Long,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assets {
    /// File name of the PROM image under `--assets`, or none for a machine
    /// that boots iris's embedded PROM.
    #[serde(default)]
    pub prom: Option<String>,
    /// File name of the prepared raw disk image under `--assets`.
    pub disk: String,
    /// SCSI id the disk is attached to.
    #[serde(default = "one")]
    pub disk_id: u8,
}

fn one() -> u8 { 1 }

/// Wait for `expect` on the serial console, then optionally type `send`.
/// Phases match in order, each after the previous one's match, so a string
/// the console prints twice names two phases.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase {
    pub name: String,
    /// Plain substring, not a pattern: console text is full of regex
    /// metacharacters.
    pub expect: String,
    /// Seconds allowed since the previous phase, before scaling.
    pub timeout: u64,
    /// Typed (with a carriage return) once the phase has matched.
    #[serde(default)]
    pub send: Option<String>,
    /// Seconds to wait before typing `send`, for prompts that drop input
    /// typed the instant they appear.
    #[serde(default)]
    pub delay: u64,
}

/// One check. Exactly one of `run` (a command in the guest) or `monitor` (a
/// command on the emulator's monitor port) is set.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub name: String,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub monitor: Option<String>,
    /// Seconds, before scaling.
    #[serde(default = "sixty")]
    pub timeout: u64,
    /// Every regex must match somewhere in the output.
    #[serde(default)]
    pub expect: Vec<String>,
    /// No regex may match.
    #[serde(default)]
    pub reject: Vec<String>,
    /// Required guest exit status (`run` steps only).
    #[serde(default)]
    pub rc: i32,
}

fn sixty() -> u64 { 60 }

/// Text the run must not print. `max` occurrences are tolerated.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forbid {
    pub pattern: String,
    #[serde(default)]
    pub max: u64,
}

/// How the run ends. With a `command`, iris must exit by itself afterwards.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finish {
    pub command: String,
    /// Seconds for iris to exit after `command`, before scaling.
    pub exit_timeout: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub speed: Speed,
    /// Guest shell the console login lands in (`csh` or `sh`).
    #[serde(default = "csh")]
    pub shell: String,
    pub assets: Assets,
    /// Merged into the generated `iris.toml`. Strings may use `{run}` for the
    /// run directory.
    pub iris: toml::Table,
    #[serde(default, rename = "phase")]
    pub phases: Vec<Phase>,
    #[serde(default, rename = "step")]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub forbid: Vec<Forbid>,
    #[serde(default)]
    pub finish: Option<Finish>,
}

fn csh() -> String { "csh".into() }

impl Scenario {
    pub fn parse(text: &str) -> Result<Scenario, String> {
        let s: Scenario = toml::from_str(text).map_err(|e| e.to_string())?;
        s.validate()?;
        Ok(s)
    }

    pub fn validate(&self) -> Result<(), String> {
        let ok_name = |n: &str| {
            !n.is_empty() && n.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        };
        // The name becomes part of a directory name.
        if !ok_name(&self.name) {
            return Err(format!("name {:?}: use letters, digits and - _ . only", self.name));
        }
        if self.phases.is_empty() {
            return Err("no [[phase]]: the run would never learn the guest is up".into());
        }
        if !matches!(self.shell.as_str(), "csh" | "sh") {
            return Err(format!("shell {:?}: expected csh or sh", self.shell));
        }
        for k in HARNESS_KEYS {
            if self.iris.contains_key(*k) {
                return Err(format!(
                    "[iris] sets {k}, which the harness owns (use [assets] for the PROM and disk)"
                ));
            }
        }
        let mut seen = BTreeSet::new();
        for p in &self.phases {
            if !seen.insert(p.name.as_str()) {
                return Err(format!("phase {:?} appears twice", p.name));
            }
            if p.expect.is_empty() || p.timeout == 0 {
                return Err(format!("phase {:?}: needs an expect string and a timeout", p.name));
            }
        }
        let mut seen = BTreeSet::new();
        for s in &self.steps {
            if !seen.insert(s.name.as_str()) {
                return Err(format!("step {:?} appears twice", s.name));
            }
            if s.run.is_some() == s.monitor.is_some() {
                return Err(format!("step {:?}: set exactly one of run and monitor", s.name));
            }
            if s.timeout == 0 {
                return Err(format!("step {:?}: timeout must be positive", s.name));
            }
            for re in s.expect.iter().chain(&s.reject) {
                super::check::compile(re).map_err(|e| format!("step {:?}: {re:?}: {e}", s.name))?;
            }
        }
        for f in &self.forbid {
            if f.pattern.is_empty() {
                return Err("forbid: empty pattern".into());
            }
        }
        if let Some(f) = &self.finish {
            if f.exit_timeout == 0 {
                return Err("finish: exit_timeout must be positive".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        name = "m"
        speed = "quick"
        [assets]
        disk = "d.raw"
        [iris.machine]
        profile = "p"
        [[phase]]
        name = "login"
        expect = "login:"
        timeout = 100
    "#;

    fn with(extra: &str) -> String { format!("{MINIMAL}\n{extra}") }

    #[test]
    fn a_minimal_scenario_parses_with_defaults() {
        let s = Scenario::parse(MINIMAL).unwrap();
        assert_eq!(s.speed, Speed::Quick);
        assert_eq!(s.shell, "csh");
        assert_eq!(s.assets.disk_id, 1);
        assert!(s.assets.prom.is_none() && s.finish.is_none());
    }

    #[test]
    fn unknown_keys_are_refused_so_a_typo_cannot_skip_a_check() {
        let e = Scenario::parse(&with("[[step]]\nname = \"x\"\nrun = \"true\"\nexpct = [\"a\"]")).unwrap_err();
        assert!(e.contains("expct"), "{e}");
    }

    #[test]
    fn a_step_needs_exactly_one_of_run_and_monitor() {
        let both = with("[[step]]\nname = \"x\"\nrun = \"a\"\nmonitor = \"b\"");
        let neither = with("[[step]]\nname = \"x\"");
        assert!(Scenario::parse(&both).unwrap_err().contains("exactly one"));
        assert!(Scenario::parse(&neither).unwrap_err().contains("exactly one"));
    }

    #[test]
    fn a_bad_regex_is_found_before_any_guest_boots() {
        let e = Scenario::parse(&with("[[step]]\nname = \"x\"\nrun = \"a\"\nexpect = [\"(\"]")).unwrap_err();
        assert!(e.contains("step \"x\""), "{e}");
    }

    #[test]
    fn the_scenario_cannot_take_over_harness_owned_keys() {
        for key in ["prom", "ci_socket", "scsi", "monitor_port"] {
            let t = MINIMAL.replace("[iris.machine]", &format!("[iris]\n{key} = 1\n[iris.machine]"));
            let e = Scenario::parse(&t).unwrap_err();
            assert!(e.contains(key), "{key}: {e}");
        }
    }

    #[test]
    fn names_are_safe_to_use_in_a_path() {
        for bad in ["", "a/b", "..x/y", "a b"] {
            let t = MINIMAL.replace("name = \"m\"", &format!("name = {bad:?}"));
            assert!(Scenario::parse(&t).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn duplicate_phase_names_and_a_run_without_phases_are_refused() {
        let dup = with("[[phase]]\nname = \"login\"\nexpect = \"x\"\ntimeout = 1");
        assert!(Scenario::parse(&dup).unwrap_err().contains("twice"));
        let none = MINIMAL.split("[[phase]]").next().unwrap();
        assert!(Scenario::parse(none).unwrap_err().contains("phase"));
    }
}
