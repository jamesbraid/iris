//! The engine's retrace clock: one retrace each field, phase anchored where the caller says.

use sgi_odyssey::retrace::RetraceClock;

/// A mode of two fields (field-sequential stereo, interlace) retraces at each field, twice a frame; one of a
/// single field once. It retraced once a frame whatever the fields.
#[test]
fn a_mode_of_two_fields_retraces_twice_a_frame() {
    let mut c = RetraceClock::default();
    c.set_mode(100_000_000, 2, 1_000_000);
    c.restart(1_000);
    // A frame is 10 ms; a field retrace falls every 5 ms from the epoch.
    assert_eq!(c.retrace_time(0), 1_000);
    assert_eq!(c.retrace_time(1) - c.retrace_time(0), 5_000_000);
    assert_eq!(c.retrace_time(2) - c.retrace_time(0), 10_000_000);
    let mut due = 0;
    while c.next_retrace(1_000 + 10_000_000) {
        due += 1;
    }
    assert_eq!(due, 2, "two retraces by the end of the first frame");
    c.set_mode(100_000_000, 1, 1_000_000);
    assert_eq!(c.retrace_time(1) - c.retrace_time(0), 10_000_000);
}

/// The phase is anchored where the caller puts it, so the retrace times a guest sees do not depend on when the
/// shell polled.
#[test]
fn a_restart_anchors_the_phase_where_it_is_given_however_late_the_polls() {
    let run = |polls: &[i64]| {
        let mut c = RetraceClock::default();
        c.set_mode(100_000_000, 1, 1_000_000);
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
