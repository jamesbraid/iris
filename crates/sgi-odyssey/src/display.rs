//! `src/display/marks.rs`, the part of iris's display the engine uses, and
//! the timing record the scan-out reports.

#[path = "../../../src/display/marks.rs"]
mod marks;

pub use marks::{Mark, Marks, Rect};

#[path = "../../../src/display/timing.rs"]
mod timing;

pub use timing::VideoTiming;
