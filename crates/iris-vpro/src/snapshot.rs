//! The TOML state helpers of iris's `snapshot`, which `dev/w1` and the
//! board's NIC save state read.

#[path = "../../../src/snapshot/state.rs"]
mod state;

pub use state::*;
