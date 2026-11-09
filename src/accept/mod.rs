//! Acceptance scenarios: boot a machine from a prepared disk and check what
//! the matrix of "does this machine still work" asks. The driver is the
//! `iris-accept` binary; everything here is the part that needs no guest, so
//! `cargo test --lib` covers it without any SGI media.
//!
//! Nothing here names a machine. See [`scenario`] for the file format.

pub mod check;
pub mod config;
pub mod result;
pub mod safety;
pub mod scenario;

#[cfg(test)]
mod shipped_scenarios {
    use super::scenario::Scenario;

    /// Every scenario under `test/accept/` parses and validates, so a typo in
    /// one is caught by `cargo test` on a machine with no disk at all.
    #[test]
    fn every_shipped_scenario_is_valid() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("test/accept");
        let mut n = 0;
        for e in std::fs::read_dir(&dir).expect("test/accept").flatten() {
            if e.path().extension().is_some_and(|x| x == "toml") {
                let text = std::fs::read_to_string(e.path()).unwrap();
                Scenario::parse(&text).unwrap_or_else(|err| panic!("{}: {err}", e.path().display()));
                n += 1;
            }
        }
        assert!(n > 0, "no scenarios in {}", dir.display());
    }
}
