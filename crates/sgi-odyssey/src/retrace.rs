//! The retrace on the clock the shell's calls carry: no refresh thread paces it in this crate.

const NS_PER_SECOND: u64 = 1_000_000_000;

/// a * b / c without overflowing the product.
fn mul_div(a: u64, b: u64, c: u64) -> u64 {
    ((a as u128 * b as u128) / c as u128) as u64
}

/// The mode's frame period on the caller's clock. A retrace falls once a frame, however many fields it has.
#[derive(Debug)]
pub struct RetraceClock {
    clock_hz: u32,
    frame_clocks: u64,
    /// Virtual ns of the next retrace; -1 disarmed.
    deadline: i64,
    /// Virtual time of retrace 0.
    epoch: i64,
    /// Retraces since the epoch.
    frames: u64,
}

impl Default for RetraceClock {
    fn default() -> Self {
        RetraceClock { clock_hz: 0, frame_clocks: 0, deadline: -1, epoch: 0, frames: 0 }
    }
}

impl RetraceClock {
    pub fn set_mode(&mut self, clock_hz: u32, frame_clocks: u64) {
        (self.clock_hz, self.frame_clocks) = (clock_hz, frame_clocks);
    }

    /// When the next retrace is due, if one is armed.
    pub fn deadline(&self) -> Option<u64> {
        (self.deadline >= 0).then_some(self.deadline as u64)
    }

    /// Virtual time of retrace `n` of the current mode, or -1 with no retrace.
    pub fn retrace_time(&self, n: u64) -> i64 {
        let hz = self.clock_hz as u64;
        let fc = self.frame_clocks;
        if hz == 0 || fc == 0 {
            return -1;
        }
        (self.epoch as u64)
            .wrapping_add((n / hz).wrapping_mul(fc).wrapping_mul(NS_PER_SECOND))
            .wrapping_add(mul_div((n % hz).wrapping_mul(fc), NS_PER_SECOND, hz)) as i64
    }

    /// Whether a retrace is due at `now_ns`; one that is counts and arms the next, so a caller that fell
    /// behind gets each in turn.
    pub fn next_retrace(&mut self, now_ns: i64) -> bool {
        if self.deadline < 0 || now_ns < self.deadline {
            return false;
        }
        self.frames += 1;
        self.arm();
        true
    }

    fn arm(&mut self) {
        self.deadline = self.retrace_time(self.frames + 1).max(-1);
    }

    /// Anchor the phase at `now_ns`.
    pub fn restart(&mut self, now_ns: i64) {
        self.epoch = now_ns;
        self.frames = 0;
        self.arm();
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }
}
