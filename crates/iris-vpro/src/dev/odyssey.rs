//! The files of `src/dev/odyssey` the back end is made of, in place.

#[path = "../../../../src/dev/odyssey/bits.rs"]
pub mod bits;
#[path = "../../../../src/dev/odyssey/consts.rs"]
pub mod consts;
#[path = "../../../../src/dev/odyssey/field.rs"]
pub mod field;
#[path = "../../../../src/dev/odyssey/vram.rs"]
pub mod vram;
#[path = "../../../../src/dev/odyssey/view.rs"]
pub mod view;
#[path = "../../../../src/dev/odyssey/logic.rs"]
pub mod logic;
#[path = "../../../../src/dev/odyssey/depth_word.rs"]
pub mod depth_word;
#[path = "../../../../src/dev/odyssey/clip.rs"]
pub mod clip;
#[path = "../../../../src/dev/odyssey/assembly.rs"]
pub mod assembly;
#[path = "../../../../src/dev/odyssey/gpu_unit.rs"]
pub mod gpu_unit;
#[path = "../../../../src/dev/odyssey/pipe/mod.rs"]
pub mod pipe;
#[path = "../../../../src/dev/odyssey/wgpu/mod.rs"]
pub mod wgpu;

pub mod back;
