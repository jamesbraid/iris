//! Judging output: console phases, step results and forbidden text. Pure
//! functions over bytes and strings, so none of it needs a guest to test.

use regex::{Regex, RegexBuilder};

use super::scenario::{Forbid, Step};

/// Where the next console phase may match: the end of the previous match.
/// Returns the offset just past the first occurrence of `needle` at or after
/// `from`, or None. The console is bytes, not UTF-8.
pub fn find_after(log: &[u8], from: usize, needle: &str) -> Option<usize> {
    let n = needle.as_bytes();
    if n.is_empty() || from > log.len() || log.len() - from < n.len() {
        return None;
    }
    log[from..].windows(n.len()).position(|w| w == n).map(|i| from + i + n.len())
}

/// Patterns see the output a line at a time: `^` and `$` anchor at line
/// breaks, since the output of a guest command is lines.
pub fn compile(pattern: &str) -> Result<Regex, regex::Error> {
    RegexBuilder::new(pattern).multi_line(true).build()
}

/// What is wrong with one step's output, one message per problem.
pub fn check_step(step: &Step, output: &str, rc: Option<i32>) -> Vec<String> {
    let mut bad = Vec::new();
    // Validated at load, so a failure to compile here is a harness bug and
    // is reported rather than hidden.
    let re = |p: &str| compile(p).map_err(|e| format!("pattern {p:?}: {e}"));
    for p in &step.expect {
        match re(p) {
            Ok(r) if !r.is_match(output) => bad.push(format!("expected /{p}/ not in output")),
            Err(e) => bad.push(e),
            _ => {}
        }
    }
    for p in &step.reject {
        match re(p) {
            Ok(r) if r.is_match(output) => bad.push(format!("rejected /{p}/ found in output")),
            Err(e) => bad.push(e),
            _ => {}
        }
    }
    if step.run.is_some() && rc != Some(step.rc) {
        bad.push(match rc {
            Some(got) => format!("guest exit status {got}, expected {}", step.rc),
            None => format!("guest exit status unknown, expected {}", step.rc),
        });
    }
    bad
}

/// Occurrences of each forbidden pattern (a plain substring) in `text`.
pub fn count_forbidden(text: &str, forbid: &[Forbid]) -> Vec<(String, u64, u64)> {
    forbid
        .iter()
        .map(|f| (f.pattern.clone(), text.matches(f.pattern.as_str()).count() as u64, f.max))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(expect: &[&str], reject: &[&str], rc: i32) -> Step {
        Step {
            name: "s".into(),
            run: Some("c".into()),
            monitor: None,
            timeout: 1,
            expect: expect.iter().map(|s| s.to_string()).collect(),
            reject: reject.iter().map(|s| s.to_string()).collect(),
            rc,
        }
    }

    #[test]
    fn phases_match_in_order_so_a_repeated_string_names_two_phases() {
        let log = b"menu\nlogin: root\nlogin:";
        let first = find_after(log, 0, "login:").unwrap();
        assert_eq!(&log[first - 6..first], b"login:");
        let second = find_after(log, first, "login:").unwrap();
        assert!(second > first);
        assert_eq!(find_after(log, second, "login:"), None);
    }

    #[test]
    fn find_after_is_safe_at_the_edges() {
        assert_eq!(find_after(b"abc", 3, "c"), None);
        assert_eq!(find_after(b"abc", 9, "c"), None);
        assert_eq!(find_after(b"abc", 0, ""), None);
        assert_eq!(find_after(b"a\xffbc", 0, "bc"), Some(4));
    }

    #[test]
    fn a_step_passes_when_every_expectation_holds() {
        let s = step(&[r"^1 \d+ MHZ IP32 Processor$"], &["error"], 0);
        assert!(check_step(&s, "CPU: R5000\n1 180 MHZ IP32 Processor\n", Some(0)).is_empty());
    }

    #[test]
    fn every_failed_expectation_is_reported() {
        let s = step(&["alpha", "beta"], &["gamma"], 0);
        let bad = check_step(&s, "gamma", Some(3));
        assert_eq!(bad.len(), 4, "{bad:?}");
    }

    #[test]
    fn exit_status_is_only_judged_for_guest_commands() {
        let mut s = step(&[], &[], 0);
        s.run = None;
        s.monitor = Some("l2 probe 0".into());
        assert!(check_step(&s, "", None).is_empty());
    }

    #[test]
    fn forbidden_text_is_counted_against_its_allowance() {
        let f = vec![
            Forbid { pattern: "PANIC".into(), max: 0 },
            Forbid { pattern: "Bus error".into(), max: 2 },
        ];
        let got = count_forbidden("Bus error\nBus error\nBus error", &f);
        assert_eq!(got, vec![("PANIC".into(), 0, 0), ("Bus error".into(), 3, 2)]);
    }
}
