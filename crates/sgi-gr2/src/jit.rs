//! The shader store the RE3 JIT keeps its compiled spans in. This crate builds its own copy of iris's
//! file, so its store is not the IMPACT crate's.

#[path = "../../../src/jit/shader_store.rs"]
pub mod shader_store;
