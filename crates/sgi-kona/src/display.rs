//! The part of iris's `display` the board names: the head it hands the refresh thread, and the frame
//! types it answers with. The thread that would drive it is iris's; here a head only holds its screen.

#[path = "../../../src/display/timing.rs"]
mod timing;

pub use timing::VideoTiming;

use std::sync::{Arc, Weak};

use parking_lot::Mutex;

use crate::disp::Rex3Screen;
use crate::gfx_display::DisplayStop;
use crate::status::MachineStatus;

pub trait Renderer: Send {
    fn stop(&mut self) {}
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Want {
    pub readback: bool,
    pub gpu: bool,
}

pub enum Composed {
    Frame,
    #[cfg(feature = "wgpu")]
    Gpu { frame: ::wgpu::Buffer, width: u32, height: u32 },
    Unchanged,
    NotReady,
}

pub trait Head: Send + Sync {
    fn vblank_start(&self, _field: u32, _t_ns: u64) {}
    fn changed(&self) -> bool {
        false
    }
    fn compose(&self, screen: &mut Rex3Screen, want: Want) -> Composed;
}

pub struct Refresh;

impl Refresh {
    pub fn head(&self, _status: Arc<MachineStatus>) -> Arc<HeadHandle> {
        Arc::new(HeadHandle {
            screen: Mutex::new(Rex3Screen::new()),
            renderer: Mutex::new(None),
            stop: DisplayStop::default(),
        })
    }
}

pub struct HeadHandle {
    screen: Mutex<Rex3Screen>,
    renderer: Mutex<Option<Box<dyn Renderer>>>,
    stop: DisplayStop,
}

impl HeadHandle {
    pub fn screen(&self) -> &Mutex<Rex3Screen> {
        &self.screen
    }
    pub fn renderer_slot(&self) -> &Mutex<Option<Box<dyn Renderer>>> {
        &self.renderer
    }
    pub fn display_stop(&self) -> &DisplayStop {
        &self.stop
    }
    pub fn set_status(&self, _status: Arc<MachineStatus>) {}
    pub fn damage(&self) {}
    pub fn attach(&self, _head: Weak<dyn Head>) {}
    pub fn set_timing(&self, _timing: VideoTiming) -> bool {
        true
    }
    pub fn start(&self) {}
    pub fn stop(&self) {}
    pub fn request_screenshot(&self) {}
    pub fn screenshot_pending(&self) -> bool {
        false
    }
    pub fn request_frame(&self) {}
    pub fn frame_pending(&self) -> bool {
        false
    }
    pub fn set_capture(&self, _on: bool) {}
}
