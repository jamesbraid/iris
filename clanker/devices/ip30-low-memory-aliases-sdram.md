# The Octane's 16 KiB at physical 0 is the first 16 KiB of SDRAM

Status: rule (HEART revision B and later, which covers every Octane we model;
DMA reach is unknown).

**What to know.** Physical 0..0x4000 is the first 16 KiB of SDRAM, offset 0
of the memory that starts at 0x2000_0000. The processor reaches those bytes
only through the low window: its access to 0x2000_0000..0x2000_4000 is a bus
error, and SDRAM is reachable at its own address from 0x2000_4000. The stock
IDE diagnostics' expectations are exactly this. 0x4000 up to the 64 KiB decode
slot is a bus error too.

In `src/dev/heart_mem.rs`, the decode slot at 0x2000_0000 is `SdramGuard`,
which faults 0x2000_0000..+16 KiB on the bus path, and the slot at 0 is
`LowAlias`, which reaches the same bytes through the unguarded decode. Both
use the store and jitv2 page generations of whichever bank the descriptors
put at 0x2000_0000. `Ip30Bus` sends the
direct paths (`mem_ptr`, block reads and writes, `gen_ptr`) over the guard
to `SdramGuard` too, since `Physical` would take the window there. The
window itself maps the guard (a stated limitation, pinned in
`src/ip30/tests.rs`). The kernel writes its exception vectors at physical 0, so a
store must advance the counter of the SDRAM page behind it or the JIT keeps
running stale code.

**Unknown.** Whether bus masters can reach 0x2000_0000..+16 KiB. The model
leaves DMA as it was: the `dma_view` reaches those bytes, the same ones the
low window shows. Public software only uses 0x400 (the GDA, OpenBSD's
`ip30.h`) and 0x600 (the CPUs' MPCONF records, Linux's `ip30-smp.c`).
