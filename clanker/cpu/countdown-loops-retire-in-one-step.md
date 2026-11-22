# The interpreter retires a counted delay loop in one step

Status: design, interpreter and `lightning` only (2026-11-21). jitv2's compiled
code does not do this.

**Symptom.** NetBSD's sgimips console clear on a GR2 Indigo2 calls its
`delay()` about 2100 times for 100000 us and thousands of times for shorter
ones, 240 s of guest time. `delay()` is a counting loop sized for 75 passes per
microsecond, so each long call is 7.5 million passes of

    addiu v0,v0,-1 ; bgtz v0,-2 ; nop

and the interpreter, at about 25 ns a pass, takes 3x the time asked. The
screen stays black for 12 minutes on a plain build, 8 on `lightning`.

**What it does.** A taken `bgtz r` or `bne r,zero` whose offset is -2 (decrement
in front of the branch, `nop` in the slot) or -1 (branch to itself, decrement in
the slot) calls `MipsExecutor::skip_countdown`. It reads the neighbouring
words with `debug_fetch_instr`, so it sees what the CPU would execute, and when
they are `addiu r,r,-k` and `nop` it takes all the passes but the last off
`r` in one go (`countdown::passes_left`), adds their instructions to
`hot.cycles` (and to `parked_cycles`, so the status bars' MIPS leave them out)
and lets the branch complete as usual. The last pass, the exit and any
interrupt run the ordinary way. Under `lightning` the fused `bne`+`nop` handler
calls it too. `IRIS_NO_COUNTDOWN_SKIP=1` turns it off.

**Why the result is indistinguishable.** A pass reads and writes `r` and
nothing else, so the state after `m` skipped passes is exactly a state the loop
reaches (same `r`, PC on the delay slot of a taken branch, `in_delay_slot` set,
same instruction count). An interrupt taken there has EPC on the branch and BD
set as in the unskipped run, and returns into a loop that finishes the same.
Tests in `mips_exec_test.rs` run all four shapes with and without the skip and
compare every GPR, PC, delay-slot state and cycle count; stop both at one
counter value and interrupt them there; and, under `ci_clock`, check that the
Compare interrupt is taken on the same instruction (the skip stops one
instruction short of `count_fire_cycle`). Not skipped: a counter that is not a
32-bit `addiu` result, `bne` that would wrap, any other register or a slot
that does work, a branch in a delay slot, fewer than 32 passes, and any build
with a PC breakpoint armed.

**Time.** Count and every device timer follow the host clock, so a skipped
loop takes about no host time, and the guest sees `r` change and no clock
move. For this loop that is a CPU of unbounded speed, the same effect jitv2
has at a few times real speed, and it is why the GR2 console clear is done in
seconds. Under `ci_clock` Count follows `hot.cycles`, so it advances by the
loop's exact instruction count. Nothing can be capped at "the next
deadline" in the host-clock build: an interrupt from another thread arrives at
any host instant and is taken at the next instruction boundary, as before.

**What it makes shorter, and what is unproven.**
- A guest that waits on a device by counting (poll, then delay, then give
  up) gets the delay with no host time for the device model to finish in.
  The device models that run on host threads (SCSI, GR2) never saw such a
  guest fail here, but a model that needs 1 ms of host time to answer a
  100 us poll loop now sees it end first. Same class as
  `irix/delay-loop-calibrated-cold.md`, taken to the limit.
- A guest that calibrates a loop against a host-clock source (HEART, the IOC
  timers, jiffies) and then sizes its delays from it measures about zero time
  per pass. IRIX 6.5.22 on the Indy and its PROM only reach the skip for
  512-pass loops (10 skips in a whole boot), so that boot does not test this
  path. Linux's `__delay` calibration is the case to try.
- jitv2: a compiled loop runs as native code and never enters the handlers.
  A hook there has to be emitted at the loop's back edge with the guest
  registers flushed around it; not done.
- Loops with other bodies (`subu r,r,1`, a `bnez` against another register, two
  counters) run as before.

**Rejected.** Scaling guest time (a faster Count) does not shorten a loop
whose pass count is a constant in the guest, and moves Count against every
other clock. Advancing only Count by the skipped time leaves the RTC and
the 8254 behind it and makes Count run ahead of the host clock for good.
Pacing the skipped loop at hardware speed in host time (sleep, wake on an
interrupt, as `idle_park` does) is exact but gives back the whole gain, and
costs jitv2 builds a 5x slowdown on this loop.

**Measured.** NetBSD 11.99.11 installer on a GR2 Indigo2, boot command to the
language menu, `loop` profile builds on a busy host (load 100-200), seconds:

| build | before | after |
|---|---|---|
| default (interpreter) | 763 | 81, 132 |
| `lightning` | 490 | 60 |

IRIX 6.5.22 on the Indy (stock image, `iris-ci boot-test`): serial login and
graphics login match the golden on both builds with the change.
