# Idle parking: status, what was tried, what is left

Branch `idle-park-proto` parks a CPU thread that is running a guest idle loop
and wakes it when the loop could do something different. This note is the
record to pick the work up from. The short design note is
[idle-park-proves-the-spin.md](idle-park-proves-the-spin.md). The argument
itself is in the header of `src/cpu/idle_park.rs`.

## Status

The branch is four commits on `cpu-compare-stop-fix` (a Compare that a CPU
stop or Count write steps over), which sits on upstream `main`. It is not
upstream yet for three reasons:

- The diff is large: +1226/-1128 lines, most of it a rewrite of
  `idle_park.rs` and the probe in `mips_exec.rs`.
- An idle IP22 (Indigo2) on an Apple M2 Max still burns 22-25 % of a host core
  where we expected 1-2 %. Not investigated.
- Ping to the NAT gateway averages 0.4 ms more on the Tezro. That is a bound
  of the design (see Known issues), not a bug.

Build and control:

- `cargo build --release --features idle-pause`. Off by default. The PC
  histogram is a separate `idleprof` feature because it costs a check per
  instruction.
- `IRIS_NO_IDLE` (any value) starts with parking off. `debug.no_idle` in the
  config does the same.
- Monitor: `idle on`, `idle off`, `idle status`. `status` prints parks, wakes
  by cause and probe refusals. It does not pause the CPU.
- `IRIS_IDLE_WARMUP_SECS` (default 60) delays the first offer.
  `IRIS_IDLE_POLL_US` (default 200) sets the memory-compare period of a parked
  CPU.

## Design and why it is exact

Skipping instructions is only safe if skipping them is unobservable. We park
a loop only when a probe has shown that running it changes nothing that
matters.

The old PC-sampling detector survives as a filter. It offers a candidate loop.
Before a park the run loop runs that loop once more, one interpreted
instruction at a time, decoding each load and store before it executes, and
records the registers and every cache line the loop touches (its own code
included). It parks only if, after one period, the registers and all those
lines are back at the same values. The loop's next state then depends only on
that state, the watched lines and the interrupt lines, so not running it is the
same as running it slowly.

Stores are allowed when they undo themselves. IRIX's idle loop stores to its
stack each pass and puts the words back (a period-1 fixed point). The park
starts after the store that begins the longest store-free stretch of the
period, so a word the loop sets and clears is left cleared.

The probe refuses, and nothing is parked, on:

- any uncached access
- any exception
- a pending interrupt line
- CP0 writes other than to Status
- TLB operations, CACHE, ERET, WAIT, SC, trap instructions
- reads of Count or Random (the loop can leave by time alone)
- more than 64 distinct lines
- no return to the start state within 512 steps (the loop is doing work and
  the probe would run it slowly)

A park ends on any interrupt line, or when a watched line changes. Other
processors and devices write memory without passing through anything a
sleeping thread can wait on, so the lines are compared every 200 us. The
compare reads through the CPU's own cache window where it maps the line, and
through the bus otherwise. The probe needs no hooks in the instruction paths,
so it works in `lightning` builds and under jitv2.

The interface to the rest of the emulator is the `idle_park::PARKED` bit and
the `hot.parked_cycles` counter. Nothing else in the CPU or devices knows a
park happened.

## Tried and rejected

- **PC-sampling detector alone** (the previous implementation). It parked busy
  loops: 191,099 of 191,159 parks ended on the 1 ms timeout instead of a real
  wake. Boots were 1.6-2.7x slower and NFS 6x slower. It is where the proof
  requirement came from.
- **WAIT only.** IRIX does not execute WAIT on the R10000 family. Wakeups
  often arrive as a store to a polled word: in a 4-CPU pipe ping-pong, 44 % of
  97,482 wakes had no interrupt.
- **MONITORX/MWAITX.** Works in user mode on Zen 3 and wakes in 0.3-0.7 us. But
  the waiting thread is still charged 100 % of a CPU, so it cannot lower host
  CPU use, which is the point.
- **Wake on reservation invalidation.** iris has no cross-CPU LLbit clear to
  hang a wake on.
- **Debug-register watch.** Misses stores made through another mapping of the
  same page.
- **Generation counters.** jitv2 builds increment a per-4 KB-page counter on
  every store, which would give a cheap "did the page change" test. It is an
  option for those builds. Comparing the lines directly works in every build,
  so we used that.
- **Virtual-time skip** (jump the clock over the idle period). Count follows
  host time, so there is nothing to skip.
- **Whole-boot bisimulation** (prove the parked and unparked runs equivalent
  by replay). iris cannot replay device I/O deterministically.

## Ideas not built

- **Closed-form solver for counted and delay loops.** PROM delay loops are the
  showcase: the PROM spends its 1-CPU power-cycle waits in them. Count is
  wall-clock, so a skipped loop still owes the wall time and the solver has to
  sleep the remainder.
- **Two-speed wait.** MWAITX for short waits, a futex for long ones.
- **Certificate cache** keyed by a hash of the loop's code bytes, so a loop
  already proven is not re-proven after each wake. This attacks the +0.4 ms
  ping and the per-park cost.
- **Gang rule plus a DMA notifier**, so that when every CPU is parked the
  process can futex-sleep until a device writes memory.
- **Perceptron prefilter** in front of the probe to cut refused attempts.
- **Residency governor** that stops offering loops which keep waking at once.
- **Self-audit:** occasionally re-execute a parked period on a shadow copy and
  compare, to catch a wrong proof in the field.

## The IRIX idle loop, as observed

Measured through the probe on 6.5.30 (IP35) and 6.5 (IP30), at an idle shell:

- 12 instructions in the loop, about 68 per pass through the scheduler path.
- It polls per-CPU memory: the next-thread pointer, the local run-queue head
  and a "queued" byte.
- It reads no Count, Cause or Status, and runs no CACHE, SYNC or WAIT.
- Its stores in a pass go to the stack only and are idempotent.
- It wakes three ways: a remote store to the polled words, an interprocessor
  interrupt (a single 64-bit store), and the timer tick.

Loops that look similar and must stay out: ll/sc spinlocks, delay loops that
read Count, the panic spin, and device polls. The proof's refusal list covers
each.

## Measurements

Idle shell, iris user+system CPU over 300 s, medians of three boots per
build, parking against none. Host load not controlled.

| Machine | No parking | Parking |
|---|---|---|
| Fuel, 1 CPU | 102 % | 41 % |
| Tezro, 2 CPUs | 203 % | 72 % |
| Tezro, 4 CPUs | 404 % | 104 % |
| Octane | 101 % | 46 % (earlier probe build, not rerun) |
| IP22 Indigo2, M2 Max | 93 % | 25 % after 3 min idle |

Other results on the final build:

- Fuel login time: medians equal within noise. Per-pair differences ran from
  -12 s to +11 s either way.
- NFS: 100 MB copies checksum correctly and run as fast or faster.
- Clock drift at most 0.03 s over 300 s.
- Ping to the NAT gateway: average +0.4 ms on the Tezro.
- M2 Max Indigo2: login 165.7 s without, 157.5 s with. Serial latency
  identical, clock within 1 s. 489,087 parks at about 1200 per second, and
  every wake was an interrupt.
- Unit tests: the 36 idle tests pass 20 of 20 runs on ubuntu, macOS arm64 and
  windows-latest, under the interpreter and jitv2
  (https://github.com/jamesbraid/iris/actions/runs/42952558445).
- Deterministic benchmark: -0.02 % against base, run-to-run spread 0.10 %.
- Mutation tests: 8 of 8.

## Known issues and open questions

- **M2 idle CPU stays at 22-25 %** against the expected 1-2 %. The park rate is
  about 1200 per second, which looks like the periodic timer tick, and each
  park re-proves the loop at 0.2-0.4 ms. So the tick plus re-proof cost may
  explain most of it. Nobody has checked.
- **+0.4 ms ping** is the design's bound: a CPU that an interrupt wakes
  re-proves its loop before it parks again.
  A certificate cache would remove most of it.
- The Octane has not been measured with the final build.
- IP28, IP27 and IP32 have not been measured. Other machines on the M2 have not been booted with parking.
- Windows full-library failures that are not idle-related: net and stream
  tests, chd file locks, `process::exit` scan separators, and a jitv2
  allocation abort. `net::stream::stop_does_not_wait` also fails on macOS
  (378 frames) and on Windows.

## Revisit checklist

1. Rebase `idle-park-proto` on current upstream `main`. Rerun the 36 idle
   tests under both engines.
2. Find where the M2 Indigo2's 22-25 % goes: count parks per second by wake
   cause and time one proof. If it is the tick plus re-proof, try the
   certificate cache before anything cleverer.
3. Re-measure the Octane and the Tezro with the final build, with and without
   parking, same protocol as the table.
4. Decide whether the diff can be split (the monitor change and the probe's
   cache-window accessor are separable) before sending it upstream.
5. Pick one or two of the unbuilt ideas only if step 2 shows the cost is in
   the proof and not the tick.
