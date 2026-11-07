//! The display handle a back end's threads call, and the attach hook the board
//! gives a back end.

use super::*;
use crate::traits::Device;

const ACTIVITY: u64 = 0x8003a8;
const INT_ENABLE: u64 = 0x800230;
const INT_CAUSE: u64 = 0x800238;
const RETRACE_TAG: u64 = 0x800248;
const INT_DEST_UPPER: u64 = 0x34;
const INT_DEST_LOWER: u64 = 0x3c;
const INT_VECTORS: u64 = 0x5c;
const CAUSE_VIDEO: u32 = 8;
const GENERAL_VECTOR: u8 = 0x10;

/// A back end that notes its lifecycle calls and keeps the handle it is given.
#[derive(Default)]
struct Probe {
    events: Arc<Mutex<Vec<&'static str>>>,
    handle: Arc<Mutex<Option<DisplayHandle>>>,
}

impl Backend for Probe {
    fn job(&mut self, _port: &mut BackPort<'_>, _job: Job<'_>) -> Reply {
        Reply::Done
    }

    fn attach(&mut self, handle: DisplayHandle) {
        self.events.lock().push("attach");
        *self.handle.lock() = Some(handle);
    }

    fn reset(&mut self) {
        self.events.lock().push("reset");
    }

}

struct Rig {
    f: Fixture,
    events: Arc<Mutex<Vec<&'static str>>>,
    handle: Arc<Mutex<Option<DisplayHandle>>>,
}

impl Rig {
    fn new() -> Rig {
        let probe = Probe::default();
        let (events, handle) = (probe.events.clone(), probe.handle.clone());
        let host = TestHost::new();
        let kona = KonaBuilder::new(KonaConfig::default(), attach(&host)).backend(Box::new(probe)).build().unwrap();
        let f = Fixture { host, kona, jobs: Default::default() };
        Rig { f, events, handle }
    }

    fn handle(&self) -> DisplayHandle {
        self.handle.lock().clone().expect("attach gave a handle")
    }

    fn events(&self) -> Vec<&'static str> {
        self.events.lock().clone()
    }

    /// The driver's interrupt programming, and the video cause enabled.
    fn program_video(&self) {
        self.f.w(INT_DEST_UPPER, 0x98000);
        self.f.w(INT_DEST_LOWER, 0x180_0090);
        self.f.w(INT_VECTORS, 0x3e_2210);
        self.f.w(INT_ENABLE, 0xf7c);
    }
}

/// Starting the board attaches the back end before the thread runs, and again
/// after a reset restarts it.
#[test]
fn start_attaches_and_a_reset_attaches_again() {
    let r = Rig::new();
    assert!(r.events().is_empty(), "nothing before the board starts");
    let board = TestBoard::new(r.f.kona.clone());
    board.start();
    assert_eq!(r.events(), ["attach"]);
    r.f.kona.power_on();
    assert_eq!(r.events(), ["attach", "reset", "attach"]);
    board.stop();
}

/// A ticket made before a reset cannot complete after it: the tag register
/// and the video cause stay as the reset left them. A ticket of the new epoch
/// completes.
#[test]
fn a_ticket_from_before_a_reset_is_dropped() {
    let r = Rig::new();
    r.program_video();
    let board = TestBoard::new(r.f.kona.clone());
    board.start();
    let h = r.handle();
    let old = h.epoch();
    r.f.kona.power_on();
    r.program_video();
    h.retrace_done(old, 0x55);
    h.video_frame(old);
    assert_eq!(r.f.r(RETRACE_TAG), 0);
    assert_eq!(r.f.r(INT_CAUSE) & CAUSE_VIDEO, 0);
    assert!(r.f.host.sent().is_empty());
    let now = r.handle().epoch();
    assert_ne!(now, old);
    h.retrace_done(now, 0x56);
    assert_eq!(r.f.r(RETRACE_TAG), 0x56);
    assert_eq!(r.f.r(INT_CAUSE) & CAUSE_VIDEO, CAUSE_VIDEO);
    assert_eq!(r.f.host.sent().len(), 1);
    board.stop();
}

/// A frame raises the video cause with no tag and sends one interrupt until
/// the driver acknowledges it.
#[test]
fn a_frame_raises_video_once_without_a_tag() {
    let r = Rig::new();
    r.program_video();
    r.f.kona.start();
    let h = r.handle();
    let e = h.epoch();
    h.video_frame(e);
    h.video_frame(e);
    assert_eq!(r.f.r(INT_CAUSE) & CAUSE_VIDEO, CAUSE_VIDEO);
    assert_eq!(r.f.r(RETRACE_TAG), 0);
    assert_eq!(r.f.host.sent().len(), 1);
    assert_eq!(r.f.host.sent()[0].vector, GENERAL_VECTOR);
    r.f.w(INT_CAUSE, CAUSE_VIDEO as u32);
    h.video_frame(e);
    assert_eq!(r.f.host.sent().len(), 2);
    r.f.kona.stop();
}

/// With no destination programmed a frame sets the cause and sends nothing.
#[test]
fn a_frame_with_no_destination_sends_no_packet() {
    let r = Rig::new();
    r.f.w(INT_ENABLE, 0xf7c);
    r.f.kona.start();
    let h = r.handle();
    for _ in 0..3 {
        h.video_frame(h.epoch());
    }
    assert_eq!(r.f.r(INT_CAUSE) & CAUSE_VIDEO, CAUSE_VIDEO);
    assert!(r.f.host.sent().is_empty());
    r.f.kona.stop();
}

/// The back end's activity reaches the driver's watchdog latch.
#[test]
fn activity_reaches_the_watchdog_latch() {
    let r = Rig::new();
    r.f.kona.start();
    assert_eq!(r.f.r(ACTIVITY), 0);
    r.handle().activity();
    assert_eq!(r.f.r(ACTIVITY), 4);
    r.f.w(ACTIVITY, 4);
    assert_eq!(r.f.r(ACTIVITY), 0);
    r.f.kona.stop();
}

/// The handle holds the register file weakly: with the board gone every call
/// is a no-op, and the handle does not keep the register file alive.
#[test]
fn a_handle_outlives_the_board_harmlessly() {
    let r = Rig::new();
    r.f.kona.start();
    let h = r.handle();
    let weak = Arc::downgrade(&r.f.kona);
    drop(r);
    assert!(weak.upgrade().is_none(), "the board is freed with the handle outstanding");
    assert_eq!(h.epoch(), 0);
    h.retrace_done(0, 1);
    h.video_frame(0);
    h.activity();
}
