//! Counted busy-wait loops: the pure part of retiring `addiu r,r,-k` and a
//! backward branch on `r` in one step. The hook is `MipsExecutor::skip_countdown`;
//! see clanker/cpu/countdown-loops-retire-in-one-step.md.

/// How the loop's branch tests the counter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Test {
    /// `bgtz r`: taken while `r > 0`.
    Positive,
    /// `bne r,zero`: taken while `r != 0`.
    NonZero,
}

/// Fewer passes than this are cheaper to run than to recognise.
pub const MIN_PASSES: u64 = 32;

/// `addiu r,r,-k`: the register and `k` (1..=32768).
pub fn decrement(raw: u32) -> Option<(u32, u32)> {
    const OP_ADDIU: u32 = 9;
    let rs = (raw >> 21) & 0x1f;
    let rt = (raw >> 16) & 0x1f;
    let imm = (raw & 0xffff) as u16 as i16;
    if raw >> 26 != OP_ADDIU || rs != rt || rs == 0 || imm >= 0 {
        return None;
    }
    Some((rs, -(imm as i32) as u32))
}

/// Taken passes that follow the branch now executing, which sees `x` in the
/// counter and is taken. Each pass takes `k` off the counter and tests it
/// again, so the loop runs until the test fails; that last pass is left to
/// the interpreter. `None` when `x` is not a 32-bit value `addiu` could have
/// left or the counter would have to wrap to reach its end.
pub fn passes_left(test: Test, x: i64, k: u32) -> Option<u64> {
    if x != x as i32 as i64 || x <= 0 {
        return None;
    }
    let (x, k) = (x as u64, k as u64);
    match test {
        Test::Positive => Some((x - 1) / k),
        Test::NonZero if x % k == 0 => Some(x / k - 1),
        Test::NonZero => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn addiu(r: u32, imm: i16) -> u32 {
        (9 << 26) | (r << 21) | (r << 16) | imm as u16 as u32
    }

    /// Run the loop pass by pass: `x` is what the branch sees, and the
    /// count is the taken branches after this one.
    fn walk(test: Test, mut x: i64, k: u32) -> u64 {
        let mut n = 0;
        loop {
            x = x as i32 as i64 - k as i64;
            x = x as i32 as i64;
            let taken = match test {
                Test::Positive => x > 0,
                Test::NonZero => x != 0,
            };
            if !taken {
                return n;
            }
            n += 1;
        }
    }

    /// The closed form must match running the loop, for both tests, small
    /// and large counters and steps that do and do not divide them.
    #[test]
    fn passes_left_matches_running_the_loop() {
        for k in [1u32, 2, 3, 7, 16, 100] {
            for x in 1..400i64 {
                let positive = passes_left(Test::Positive, x, k).unwrap();
                assert_eq!(positive, walk(Test::Positive, x, k), "bgtz x={x} k={k}");
                match passes_left(Test::NonZero, x, k) {
                    Some(n) => assert_eq!(n, walk(Test::NonZero, x, k), "bne x={x} k={k}"),
                    None => assert_ne!(x % k as i64, 0, "bne x={x} k={k}"),
                }
            }
        }
    }

    /// A counter outside what `addiu` leaves in a register, or at or below
    /// zero, is not a countdown to skip.
    #[test]
    fn passes_left_refuses_what_a_countdown_cannot_be() {
        assert_eq!(passes_left(Test::Positive, 0, 1), None);
        assert_eq!(passes_left(Test::Positive, -5, 1), None);
        assert_eq!(passes_left(Test::NonZero, -5, 1), None);
        assert_eq!(passes_left(Test::Positive, 0x1_0000_0000, 1), None);
        assert_eq!(passes_left(Test::Positive, 0xffff_ffff, 1), None);
        assert_eq!(passes_left(Test::Positive, 0x7fff_ffff, 1), Some(0x7fff_fffe));
    }

    /// Only `addiu r,r,-k` with r not $zero counts down.
    #[test]
    fn decrement_takes_only_a_self_decrementing_addiu() {
        assert_eq!(decrement(addiu(2, -1)), Some((2, 1)));
        assert_eq!(decrement(addiu(8, i16::MIN)), Some((8, 32768)));
        assert_eq!(decrement(addiu(2, 1)), None);
        assert_eq!(decrement(addiu(2, 0)), None);
        assert_eq!(decrement(addiu(0, -1)), None);
        assert_eq!(decrement(addiu(2, -1) & !(0x1f << 16) | (3 << 16)), None, "addiu r3,r2,-1");
        assert_eq!(decrement(0), None);
    }
}
