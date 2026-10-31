//! The guest GL suites' baselines: which `pixtest` and `glstate` cases pass
//! on the VPro, and a check that none of them stops passing.
//!
//! The suites (`test/gltest/pixtest.c`, `glstate.c`) run inside the IRIX
//! guest and print one line a case, `PASS #12 read/RGBA/UNSIGNED_BYTE`.
//! `testdata/gltest/<suite>.pass` lists the cases that passed when the
//! baseline was taken, `id name` a line. A case counts as passing for the
//! baseline when it printed `PASS` or `XPASS` (a case expected to fail that
//! passed), which is how the suites' own "passed" total counts. The check
//! keys on the name, since ids shift when a case is added.
//!
//! `VPRO_GLTEST_RESULTS=<dir>` with the suites' output files in it
//! (`o_pixtest.out`, `o_pix8.out`, `o_pix12.out`, `o_glstate.out`, which
//! the guest run leaves) makes `guest_gl_suites_keep_their_baselines` compare them;
//! `VPRO_GLTEST_BLESS=1` with `gltest_bless` rewrites the baselines from them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// (baseline file stem, the suite's output file)
pub const SUITES: [(&str, &str); 4] = [("pixtest", "o_pixtest.out"), ("pixtest-ci8", "o_pix8.out"), ("pixtest-ci12", "o_pix12.out"), ("glstate", "o_glstate.out")];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Case {
    pub id: u32,
    pub name: String,
    pub status: String,
}

/// The cases of one suite's output: lines `STATUS #id name[: detail]`.
///
/// A name some cases share (the suite repeats a few) is told apart by its
/// order: the second occurrence is `name [2]`.
pub fn parse(text: &str) -> Vec<Case> {
    let mut seen: BTreeMap<String, u32> = BTreeMap::new();
    text.lines()
        .filter_map(|l| {
            let (status, rest) = l.split_once(' ')?;
            if !matches!(status, "PASS" | "FAIL" | "XFAIL" | "XPASS" | "SKIP") {
                return None;
            }
            let rest = rest.strip_prefix('#')?;
            let (id, rest) = rest.split_once(' ')?;
            let name = rest.split(": ").next().unwrap_or(rest).trim();
            let n = seen.entry(name.to_string()).or_insert(0);
            *n += 1;
            let name = if *n == 1 { name.to_string() } else { format!("{name} [{n}]") };
            Some(Case { id: id.parse().ok()?, name, status: status.to_string() })
        })
        .collect()
}

pub fn baseline_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/dev/odyssey/testdata/gltest")
}

fn load_baseline(stem: &str) -> Vec<(u32, String)> {
    let p = baseline_dir().join(format!("{stem}.pass"));
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let (id, name) = l.split_once(' ').unwrap_or_else(|| panic!("{}: {l:?}", p.display()));
            (id.parse().unwrap_or_else(|_| panic!("{}: {l:?}", p.display())), name.to_string())
        })
        .collect()
}

/// What a suite's result says against its baseline.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Baseline cases that no longer pass, with what they printed.
    pub regressed: Vec<(String, String)>,
    /// Cases that pass and the baseline does not list.
    pub gained: Vec<String>,
}

fn passes(status: &str) -> bool {
    status == "PASS" || status == "XPASS"
}

pub fn compare(baseline: &[(u32, String)], results: &[Case]) -> Verdict {
    let by_name: BTreeMap<&str, &str> = results.iter().map(|c| (c.name.as_str(), c.status.as_str())).collect();
    let mut v = Verdict::default();
    for (_, name) in baseline {
        match by_name.get(name.as_str()) {
            Some(s) if passes(s) => {}
            Some(s) => v.regressed.push((name.clone(), s.to_string())),
            None => v.regressed.push((name.clone(), "missing".to_string())),
        }
    }
    let known: std::collections::BTreeSet<&str> = baseline.iter().map(|(_, n)| n.as_str()).collect();
    for c in results {
        if passes(&c.status) && !known.contains(c.name.as_str()) {
            v.gained.push(c.name.clone());
        }
    }
    v
}

fn results_dir() -> Option<PathBuf> {
    std::env::var("VPRO_GLTEST_RESULTS").ok().map(PathBuf::from)
}

fn read_results(dir: &Path, file: &str) -> Vec<Case> {
    let p = dir.join(file);
    parse(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display())))
}

/// The suites' output from a guest run holds every case the baseline lists.
/// With no results given the test says so and passes (they need a guest);
/// `VPRO_GLTEST_REQUIRE=1` makes that a failure.
#[test]
fn guest_gl_suites_keep_their_baselines() {
    let Some(dir) = results_dir() else {
        assert!(std::env::var("VPRO_GLTEST_REQUIRE").is_err(), "VPRO_GLTEST_RESULTS is not set");
        eprintln!("SKIPPED: VPRO_GLTEST_RESULTS is not set: see testdata/README.md");
        return;
    };
    let mut bad = vec![];
    for (stem, file) in SUITES {
        let v = compare(&load_baseline(stem), &read_results(&dir, file));
        for (name, s) in &v.regressed {
            bad.push(format!("{stem}: {name} was a pass and is now {s}"));
        }
        if !v.gained.is_empty() {
            eprintln!("{stem}: {} cases pass that the baseline does not list (scripts/vpro-goldens.sh gltest-bless): {:?}", v.gained.len(), &v.gained[..v.gained.len().min(8)]);
        }
    }
    assert!(bad.is_empty(), "{} regressions:\n{}", bad.len(), bad.join("\n"));
}

/// Rewrite the baselines from a guest run's output.
#[test]
#[ignore]
fn gltest_bless() {
    assert!(std::env::var("VPRO_GLTEST_BLESS").is_ok(), "set VPRO_GLTEST_BLESS=1: blessing rewrites the baselines");
    let dir = results_dir().expect("VPRO_GLTEST_RESULTS");
    std::fs::create_dir_all(baseline_dir()).unwrap();
    for (stem, file) in SUITES {
        let cases = read_results(&dir, file);
        let pass: Vec<&Case> = cases.iter().filter(|c| passes(&c.status)).collect();
        let count = |s: &str| cases.iter().filter(|c| c.status == s).count();
        let mut out = format!(
            "# {stem}: the cases that passed on the VPro (PASS or XPASS). id name.\n# {} cases: {} PASS, {} FAIL, {} XFAIL, {} XPASS, {} SKIP.\n",
            cases.len(),
            count("PASS"),
            count("FAIL"),
            count("XFAIL"),
            count("XPASS"),
            count("SKIP")
        );
        for c in pass {
            out += &format!("{} {}\n", c.id, c.name);
        }
        std::fs::write(baseline_dir().join(format!("{stem}.pass")), out).unwrap();
        eprintln!("{stem}: {} passing cases", cases.iter().filter(|c| passes(&c.status)).count());
    }
}

/// The committed baselines read, list each case once, and say how many
/// cases they hold.
#[test]
fn the_gl_suite_baselines_are_well_formed() {
    for (stem, _) in SUITES {
        let p = baseline_dir().join(format!("{stem}.pass"));
        let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        let cases = load_baseline(stem);
        let names: std::collections::BTreeSet<&String> = cases.iter().map(|(_, n)| n).collect();
        assert_eq!(names.len(), cases.len(), "{stem}: a case is listed twice");
        let header = text.lines().nth(1).unwrap();
        let num = |after: &str, before: &str| -> usize { header.split(after).nth(1).and_then(|r| r.split(before).next()).and_then(|n| n.trim().parse().ok()).unwrap_or_else(|| panic!("{stem}: header {header:?}")) };
        let said = num(" cases: ", " PASS") + num(" XFAIL, ", " XPASS");
        assert_eq!(said, cases.len(), "{stem}: the header counts {said} passes, the list has {}", cases.len());
        assert!(!cases.is_empty());
    }
}

/// A case that stops passing, goes missing or is skipped is a regression;
/// one that starts passing is only noted.
#[test]
fn a_case_that_stops_passing_is_a_regression() {
    let baseline = vec![(1, "read/RGBA/UNSIGNED_BYTE".to_string()), (2, "read/RGBA/BYTE".to_string()), (3, "read/RGB/FLOAT".to_string()), (4, "tex/RGBA8".to_string())];
    let out = "PASS #1 read/RGBA/UNSIGNED_BYTE\n\
               FAIL #2 read/RGBA/BYTE: (0,0) comp 0 got 0.5 want 0.1\n\
               SKIP #3 read/RGB/FLOAT: extension missing\n\
               PASS #5 gettex/RGBA8/ABGR_EXT/UNSIGNED_BYTE\n\
               XFAIL #6 bitmap/skip: libGLcore does it too\n\
               pixtest: 2 passed, 1 failed, 1 expected failures, 1 skipped\n";
    let results = parse(out);
    assert_eq!(results.len(), 5);
    let twice = parse("PASS #1 a/b\nFAIL #2 a/b: x\nPASS #3 a/c\n");
    assert_eq!(twice.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["a/b", "a/b [2]", "a/c"]);
    assert_eq!(results[1], Case { id: 2, name: "read/RGBA/BYTE".into(), status: "FAIL".into() });
    let v = compare(&baseline, &results);
    assert_eq!(v.regressed, vec![("read/RGBA/BYTE".to_string(), "FAIL".to_string()), ("read/RGB/FLOAT".to_string(), "SKIP".to_string()), ("tex/RGBA8".to_string(), "missing".to_string())]);
    assert_eq!(v.gained, vec!["gettex/RGBA8/ABGR_EXT/UNSIGNED_BYTE".to_string()]);
    assert_eq!(compare(&baseline[..1], &results), Verdict { regressed: vec![], gained: vec!["gettex/RGBA8/ABGR_EXT/UNSIGNED_BYTE".to_string()] });
}
