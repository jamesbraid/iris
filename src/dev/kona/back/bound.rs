//! The most words a pixel's record may take, from the host's own budget
//! arithmetic.
//!
//! The host driver, not the board, picks a window's per-pixel class: it
//! counts the managed area's pixels (the width rounded up to a multiple of
//! 160, times the height), divides a budget of 32-bit words by that count and
//! maps the quotient to a class of 64, 32, 16, 8 or 4 words. The budget is
//! `rm_count * 5 * 2^22` words on an InfiniteReality, `2^21` per unit on the
//! Reality board set, and up to `2^27` per unit on the InfiniteReality4 board
//! set (the amount populated is unknown). That the budget counts 32-bit words
//! is inferred; the worked case agrees with it: one raster manager at
//! 1280 x 1024 gives 20,971,520 words over 1,310,720 pixels, 16 words, the
//! published 512 bits per pixel.
//!
//! The back end does not report a class: the guest derives it from the raster
//! manager count and the managed size, which the front end already supplies.
//! The bound only limits the model: a buffer layout that needs more words than
//! it allows is refused and counted.

/// Which board set the pipe carries, which sizes the pixel budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoardSet {
    /// The Reality board set: half the InfiniteReality budget.
    Reality,
    /// InfiniteReality, InfiniteReality2 and InfiniteReality3.
    InfiniteReality,
    /// InfiniteReality4: the largest budget the board set allows.
    InfiniteReality4,
}

/// The widths the host rounds the managed area's width up to.
const WIDTH_UNIT: u32 = 160;

/// The classes, in words per pixel, largest first.
const CLASSES: [u32; 5] = [64, 32, 16, 8, 4];

impl BoardSet {
    /// The budget in 32-bit words for `rm_count` raster managers.
    pub fn budget_words(self, rm_count: u32) -> u64 {
        let unit: u64 = match self {
            BoardSet::Reality => 5 << 21,
            BoardSet::InfiniteReality => 5 << 22,
            BoardSet::InfiniteReality4 => 5 << 27,
        };
        unit * rm_count as u64
    }
}

/// The managed area's pixels as the host counts them.
pub fn managed_pixels(width: u32, height: u32) -> u64 {
    width.div_ceil(WIDTH_UNIT) as u64 * WIDTH_UNIT as u64 * height as u64
}

/// The words a pixel's record may take, or 0 when the budget gives less than
/// the smallest class (which the hardware documents do not cover).
pub fn record_words(board_set: BoardSet, rm_count: u32, width: u32, height: u32) -> u32 {
    let pixels = managed_pixels(width, height);
    if pixels == 0 {
        return 0;
    }
    let quotient = board_set.budget_words(rm_count) / pixels;
    CLASSES.into_iter().find(|&c| quotient >= c as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worked case: one raster manager, 1280 x 1024, 16 words, 512 bits.
    #[test]
    fn one_raster_manager_at_1280_by_1024_is_sixteen_words() {
        assert_eq!(BoardSet::InfiniteReality.budget_words(1), 20_971_520);
        assert_eq!(managed_pixels(1280, 1024), 1_310_720);
        assert_eq!(record_words(BoardSet::InfiniteReality, 1, 1280, 1024), 16);
    }

    #[test]
    fn the_budget_scales_with_the_raster_managers() {
        for n in [1u64, 2, 4] {
            assert_eq!(BoardSet::InfiniteReality.budget_words(n as u32), n * 5 * (1 << 22));
            assert_eq!(BoardSet::Reality.budget_words(n as u32), n * 5 * (1 << 21));
            assert_eq!(BoardSet::InfiniteReality4.budget_words(n as u32), n * 5 * (1 << 27));
        }
        assert_eq!(record_words(BoardSet::InfiniteReality, 2, 1280, 1024), 32);
        assert_eq!(record_words(BoardSet::InfiniteReality, 4, 1280, 1024), 64);
        assert_eq!(record_words(BoardSet::Reality, 1, 1280, 1024), 8);
    }

    #[test]
    fn the_widest_class_is_sixty_four_words() {
        assert_eq!(record_words(BoardSet::InfiniteReality4, 4, 1280, 1024), 64);
        assert_eq!(record_words(BoardSet::InfiniteReality4, 1, 1280, 1024), 64);
    }

    /// The width rounds up to a multiple of 160 before the division.
    #[test]
    fn the_width_rounds_up_to_160() {
        assert_eq!(managed_pixels(1, 1), 160);
        assert_eq!(managed_pixels(160, 2), 320);
        assert_eq!(managed_pixels(161, 1), 320);
        // 1920 is a multiple; 1921 is not and costs a whole extra unit.
        assert_eq!(managed_pixels(1920, 1080), 1920 * 1080);
        assert_eq!(managed_pixels(1921, 1080), 2080 * 1080);
    }

    /// A quotient between two classes falls to the smaller; one below the
    /// smallest gives no class.
    #[test]
    fn a_quotient_between_classes_takes_the_lower() {
        // 2048 rounds up to 2080: 20,971,520 / 4,259,840 = 4.9, the 4-word
        // class; on the Reality board set 2.4, no class.
        assert_eq!(record_words(BoardSet::InfiniteReality, 1, 2048, 2048), 4);
        assert_eq!(record_words(BoardSet::Reality, 1, 2048, 2048), 0);
        // 1920 x 1200: 20,971,520 / 2,304,000 = 9, so the 8-word class.
        assert_eq!(record_words(BoardSet::InfiniteReality, 1, 1920, 1200), 8);
        assert_eq!(record_words(BoardSet::InfiniteReality, 1, 0, 1024), 0);
    }
}
