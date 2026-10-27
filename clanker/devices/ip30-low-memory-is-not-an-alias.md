# The Octane's 16 KiB at physical 0 is its own storage, not an alias of SDRAM

Status: rule (the model is an approximation; where the board really backs
these 16 KiB is unknown).

**What to know.** On the Indy, low memory is an alias of bank 0: it decodes
back onto the bank and shares its jitv2 generation counters (`AliasBus` in
`src/physical.rs`). Do not copy that for the Octane. `LowMemory` in
`src/dev/heart_mem.rs` is 16 KiB of distinct storage at physical 0, zero at
reset, with one generation counter per 4 KiB page of its own. Everything from
0x4000 up to the 64 KiB decode slot, and SDRAM space from 0x2000_0000 to
0x2000_4000, is a bus error. SDRAM starts at 0x2000_4000.

**Why.** Two things the stock PROM's memory diagnostic does:

- It expects 0x2000_0000..0x2000_4000 to bus-error, and the PROM starts
  allocating SDRAM above it. Aliasing low memory onto the first SDRAM pages
  would make that range answer.
- Its address-bit test writes opposing patterns to the lower and upper 8 KiB
  and expects each to keep its value. An 8 KiB mirror failed it with exactly
  4,096 mismatches, one per word of the upper half.

The kernel also writes its exception vectors here, so the JIT needs a counter
per page or it keeps running code compiled from an earlier copy.

**Unknown.** Whether the board physically aliases these 16 KiB onto some
other SDRAM location. No published source says where the backing lives.
Public software only uses 0x400 (the GDA, OpenBSD's `ip30.h`) and 0x600 (the CPUs' MPCONF records, Linux's `ip30-smp.c`).

**How to apply.** Keep low memory separate while that stays unknown. If
evidence shows where the board backs it, alias it there and share that RAM's
generation counters, as the Indy does, instead of keeping two copies. The
test `stores_advance_the_page_generation` in `src/dev/heart_mem.rs` covers the
counters; the bus fault does not apply to a cached access through the CPU's
window, so check the bus path when testing the guard.
