//! The loops a shell writes once for every board.

use crate::engine::{BoardEngine, Idle, Refused, StallOn};

/// Give a stalled engine what it waits for: `frame(now)` for a frame, an
/// `advance` at the time for a time. `now` moves up to a stall's time. Waits
/// `slice_ns` of host time at a turn, for at most `rounds` turns; returns
/// `Yes` or `Faulted`, or `Working` when the rounds ran out. A shell that
/// cannot refuse a read passes `usize::MAX`.
pub fn drain_stalls<E: BoardEngine>(e: &mut E, now: &mut u64, slice_ns: u64, rounds: usize) -> Idle {
    for _ in 0..rounds {
        match e.wait_idle(*now, slice_ns) {
            Idle::Stalled(on) => release(e, now, on),
            Idle::Working => {}
            done => return done,
        }
    }
    Idle::Working
}

/// `quiesce`, giving a stalled engine what it waits for between tries.
pub fn quiesce<E: BoardEngine>(e: &mut E, now: &mut u64, slice_ns: u64, rounds: usize) -> Result<(), Refused> {
    for _ in 0..rounds {
        match e.quiesce(slice_ns) {
            Err(Refused::Midway(_)) => {
                if let Idle::Stalled(on) = e.wait_idle(*now, 0) {
                    release(e, now, on);
                }
            }
            done => return done,
        }
    }
    Err(Refused::Midway(format!("the engine was still busy after {rounds} tries")))
}

fn release<E: BoardEngine>(e: &mut E, now: &mut u64, on: StallOn) {
    match on {
        StallOn::Frame => e.frame(*now),
        StallOn::Time(t) => {
            *now = (*now).max(t);
            e.advance(*now);
        }
    }
}
