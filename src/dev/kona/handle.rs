//! What a back end's own threads may do to the front end.
//!
//! `BackPort` reaches the front end only inside `KonaBackend::job`, on the Host
//! Interface Processor's thread. The display runs on a thread of its own, at
//! the frame rate, and has to raise the video interrupt when a frame starts. The
//! handle is how it does that. It holds the register file, which owns nothing
//! of the back end, so there is no cycle.
//!
//! A handle call takes the epoch it was made in (`epoch`). A reset begins a
//! new epoch, so a thread that was in the middle of announcing a frame when
//! the reset happened cannot raise a cause the reset cleared.

use super::front::Host;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

/// The front end as a back end's display thread may use it. Cheap to clone.
#[derive(Clone)]
pub struct DisplayHandle {
    host: Arc<Host>,
}

impl super::back::tiles::Activity for DisplayHandle {
    fn activity(&self) {
        self.host.activity.store(true, Relaxed);
    }
}

impl DisplayHandle {
    pub(super) fn new(host: &Arc<Host>) -> Self {
        DisplayHandle { host: host.clone() }
    }

    /// The epoch a ticket made now belongs to.
    pub fn epoch(&self) -> u64 {
        self.host.front.lock().epoch()
    }

    /// A frame started in `epoch`: the video interrupt rises, with no tag.
    pub fn video_frame(&self, epoch: u64) {
        self.host.with_front(|f| ((), f.video_frame(epoch)));
    }

    /// The back end is working: the driver's watchdog sees activity. For a
    /// drain loop or the raster thread while it works. Not for the display
    /// thread, which runs whether or not the pipe does: if it set the latch
    /// the watchdog could never fire.
    pub fn activity(&self) {
        self.host.activity.store(true, Relaxed);
    }
}
