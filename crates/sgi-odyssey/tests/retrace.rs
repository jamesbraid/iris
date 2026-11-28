//! The engine's retrace clock: one retrace each frame, phase anchored where the caller says.

use sgi_odyssey::retrace::RetraceClock;

/// The clock paces the frame period, 10 ms here, whatever the mode's fields.
#[test]
fn the_retrace_period_is_the_frame_period() {
    let mut c = RetraceClock::default();
    c.set_mode(100_000_000, 1_000_000);
    c.restart(1_000);
    assert_eq!(c.retrace_time(0), 1_000);
    assert_eq!(c.retrace_time(1) - c.retrace_time(0), 10_000_000);
    assert_eq!(c.retrace_time(7) - c.retrace_time(0), 70_000_000);
    let mut due = 0;
    while c.next_retrace(1_000 + 20_000_000) {
        due += 1;
    }
    assert_eq!(due, 2, "two retraces by the end of the second frame");
}

/// The phase is anchored where the caller puts it, so the retrace times a guest sees do not depend on when the
/// shell polled.
#[test]
fn a_restart_anchors_the_phase_where_it_is_given_however_late_the_polls() {
    let run = |polls: &[i64]| {
        let mut c = RetraceClock::default();
        c.set_mode(100_000_000, 1_000_000);
        c.restart(7_000_000);
        for &t in polls {
            while c.next_retrace(t) {}
        }
        (c.retrace_time(0), c.retrace_time(1), c.frames())
    };
    let (epoch, first, _) = run(&[7_000_001]);
    assert_eq!(epoch, 7_000_000);
    let (late_epoch, late_first, frames_late) = run(&[40_000_000]);
    let (steady_epoch, steady_first, frames_steady) = run(&[8_000_000, 20_000_000, 40_000_000]);
    assert_eq!((late_epoch, late_first), (epoch, first));
    assert_eq!((steady_epoch, steady_first, frames_steady), (epoch, first, frames_late));
}
