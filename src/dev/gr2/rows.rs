//! The VRAM rows (bottom-up) drawing has reached, so a scan-out composes only those.

use std::sync::atomic::{AtomicU64, Ordering};

use super::re3::FB_H;

const WORDS: usize = FB_H / 64;

/// One bit per row, filled by the thread that draws. Plain data; all-zero is empty.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct RowSet([u64; WORDS]);

impl RowSet {
    /// Mark rows `lo` to `hi`, both included, cut to the frame buffer.
    pub fn mark(&mut self, lo: i64, hi: i64) {
        for y in lo.max(0)..=hi.min(FB_H as i64 - 1) {
            self.0[y as usize / 64] |= 1 << (y % 64);
        }
    }

    /// Mark display rows `first` up to `end`.
    pub fn mark_display(&mut self, first: usize, end: usize) {
        self.mark((FB_H - end) as i64, (FB_H - 1 - first) as i64);
    }

    pub fn is_empty(&self) -> bool {
        self.0 == [0; WORDS]
    }

    pub fn has(&self, y: usize) -> bool {
        self.0[y / 64] >> (y % 64) & 1 != 0
    }

    pub(super) fn all() -> RowSet {
        RowSet([!0; WORDS])
    }

    /// Runs of marked rows, `(first, end)`, with each row `y` counted as display row `FB_H - 1 - y`, in display order.
    pub fn display_runs(&self) -> Vec<(u32, u32)> {
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for d in 0..FB_H {
            if self.has(FB_H - 1 - d) {
                match runs.last_mut() {
                    Some(r) if r.1 == d as u32 => r.1 += 1,
                    _ => runs.push((d as u32, d as u32 + 1)),
                }
            }
        }
        runs
    }
}

/// The rows marked since the last `take`, for threads to share: the drawing thread adds, the shell takes.
pub(super) struct DamagedRows([AtomicU64; WORDS]);

impl DamagedRows {
    /// Mark `set`'s rows, then empty it. Release: a taker that sees a row sees the drawing it stands for.
    pub fn add(&self, set: &mut RowSet) {
        for (w, bits) in self.0.iter().zip(set.0.iter_mut()) {
            if *bits != 0 {
                w.fetch_or(*bits, Ordering::Release);
                *bits = 0;
            }
        }
    }

    pub fn mark_all(&self) {
        for w in &self.0 {
            w.store(!0, Ordering::Release);
        }
    }

    /// The marked rows, which are cleared.
    pub fn take(&self) -> RowSet {
        let mut set = RowSet::default();
        for (w, bits) in self.0.iter().zip(set.0.iter_mut()) {
            *bits = w.swap(0, Ordering::Acquire);
        }
        set
    }
}
