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
