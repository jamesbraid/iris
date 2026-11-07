# KONA front end: the Host Interface Processor, its queue and its seam

Status: reference (iris `src/dev/kona/`, 2026-11-07). The InfiniteReality
pipe's host interface. Nothing draws yet; no machine instantiates the board.

**What it is.** The processor stores commands to a command page in the pipe's
widget aperture. A Host Interface Processor on the board takes the words,
answers the commands that are about the board itself, and hands every command
that draws or changes display state to the Geometry Distributor and the engines
behind it. This module is the Host Interface Processor and the widget around it.
The engines are a `Backend` a later change implements; `NullBackend` stands in.

## Attaching it

A machine builds `KonaBuilder::new(cfg, KonaAttach { packets, dma, fifo_low })`
and calls `bus_read(off, size)` / `bus_write(off, size, val)` with offsets inside
the 16 MiB aperture. Nothing in the module names a Hub, HEART or Bedrock type:

- `PacketSender::send(InterruptPacket { widget, address, vector })`: the pipe
  stores a vector at the address the driver programmed into its destination
  registers. Assert-only. Called with no board lock held.
- `HostDma::{read,write}(addr, buf)`: host physical addresses, as the driver's
  packets and pages name them. The packets name no destination widget; the
  machine's handle chooses one.
- `FifoLow::drained()`: the command FIFO and the words behind it are all run.
  The machine can hand back flow-control credits its processor interface holds,
  and wake a processor the driver put to sleep waiting for the pipe to drain.

The graphics credits themselves (the Hub's per-store credit counter) are not
modelled; the qtest cases for them are not ported.

## The thread and the queue

One thread, `kona-hip`, stands for the Host Interface Processor. A store to the
command page, the DMA push port, the bracket register, the firmware load port
or the map RAM goes into a `GFifo` and the thread runs it in order. A full ring
answers the bus `BUS_BUSY`; the CPU thread never spins (see
`a-refused-command-store-answers-busy-never-spins.md` in `odyssey/` for why).

`regs::classify` says how an access meets the stream:

| Class | Registers | Rule |
|---|---|---|
| Queues (stores) | command page, DMA push, bracket, load port, map RAM | joins the ring |
| Now | interrupt cause, enable, mailbox, credit (read), retrace tag, widget id, low-window word, activity latch | served from the register file at once, whatever the thread does |
| Waits | everything else | busy until the ring is empty and the thread idle |

The register at +0x800208 is the graphics FIFO's fill level: the words framers
hold plus the ring's backlog, rounded up to two (bit 0 never sets), and full
(0x7fe) while a display list runs. Users wait for it to fall; the graphics
install tool reads it before every command word and goes on below 0x5dd.

Without the thread running (before `start`, in unit tests) a stream store runs
on the caller's thread. `stop` joins the thread and runs what it had not taken;
call it with the processors stopped.

Locks: `Host::front` (register file, a few instructions, never across a call
out). The Host Interface Processor's state sits in a slot (`Shared::pipe`); the
thread, or a processor with the ring empty, takes it out for an instant, works
without any lock, and puts it back. A taken slot is busy to the bus. So the
machine's DMA, interrupt and FIFO handles are never called with a board lock
held. The activity latch, held-word count, "reasons wait" and "list runs" flags
are atomics because the thread touches them per word.

Re-entry and failure limits:

- A machine's DMA or interrupt handle is called from the kona thread (and from
  a processor's thread for waiting accesses) with the state taken out. It must
  not call back into the board's monitor accessors (`failure`, `flash_image`,
  `load_record`, `with_backend`) or reset: they wait for the state and would
  wait on the caller's own guard.
- A back-end job that never returns keeps the state taken, so it blocks those
  monitors and reset (they sleep, they do not spin) and every waiting register.
- A back-end panic on the kona thread marks the board dead. A panic on a CPU
  thread, from a waiting access that runs the back end inline (a firmware
  start, an indirect store, or a stream store with no thread running), does
  not: the guard puts the state back, the panic propagates to that caller, and
  the board stays usable with whatever state the back end left. This is a
  known limitation.

The published held-word count (and so the credit register) is updated at the
end of each run of up to 32 stores, so during a long run it lags the framers.
An 8-byte access is all or nothing: a store refused busy has done none of its
words (reads have no side effects), so the processor's retry repeats nothing.

## The command stream

`pipe.rs` ports the textport/kernel/GL framers: a word is an argument of the
open command, else it opens one. The grammars share the page; how the pipe tells
them apart is unknown, and `Core::textport_push` documents the rule the model
infers. Variable runs the front end can size (colour map, gamma, cursor, video,
EEPROM, list-call pairs, texture table) it takes itself; a run the back end must
size is asked for by answering `Reply::Data(n)` to the command.

## What a back end's threads may do

`BackPort` works only inside `Backend::job`, on `kona-hip`. A back end with a
display thread (or a raster thread) gets a `DisplayHandle` instead, from
`Backend::attach`, which `Device::start` calls before the thread runs and
again after each stop:

- `retrace_done(epoch, tag)` and `video_frame(epoch)` set the retrace tag and
  the video cause and send the general-vector packet once, with the register
  lock dropped, so they cannot deadlock the Host Interface Processor's thread.
  Both are for the display thread. `video_frame` is the tagless form.
- `activity()` sets the watchdog latch. The drain loops and the raster thread
  call it while they work; the display thread must not, or the watchdog could
  never fire.
- `epoch()` is what a ticket is made in. A reset begins a new epoch, and a
  call carrying an older one is dropped, so a thread that was mid-call during
  a reset cannot raise a cause the reset cleared.

The handle holds the register file (`Host`, its own `Arc`) weakly: the pipe
slot owns the back end, so a strong reference would be a cycle. With the board
gone every call is a no-op. With no interrupt destination programmed the video
cause still sets, and the missing destination is logged once, not per frame.

`Device::stop` calls `Backend::stop` after the thread joined and the jobs it
had not taken ran, with the slot taken and no lock held. `Kona::quiesce` lets
the ring run out and calls `Backend::quiesce`; `Kona::resume` undoes it.

## Approximations and open questions

Grades: established (a published manual or measurement), inferred (from
software's behaviour or a public driver), approximated (a model choice), unknown.
Statements about what host software does are software expectations taken from
QEMU's model (jamesbraid/qemu-sgi-origami), not hardware facts.

| Item | Grade |
|---|---|
| A DMA packet's high word with bit 28 clear is `io >> 12` of a device address the map RAM translates (entry `word >> 2`); with bit 28 set it is an absolute host frame | inferred; the entry decode is unknown |
| A mapped packet that hits an invalid entry is refused and completes nothing | unknown (the error register's bit 13 is the likely report) |
| The widget control register resets to the host widget (`KonaConfig::with_host_widget`) in bits 25:22, so DMA works before software programs it | approximated: the model's choice; the real power-on value is unknown (QEMU does the same with its hub widget) |
| The widget DMA goes to is bits 25:22 of the widget control register (`Kona::dma_widget`); the machine's handle may use it | inferred |
| GL texture table framed as a slot word and 33 words | inferred |
| Video commands of unknown length framed by searching for a checksum equal to the command plus its data words | inferred |
| Textport, kernel and GL grammars share the command page; the rule that tells them apart (`Core::textport_push`) | inferred; the pipe's own rule is unknown |
| EEPROM identify answers 0x1fc40000 every time (codes from a published example) | software assumes it is stable (established); a part in ID mode returns fixed bytes (inferred); the codes are documentation, not a measurement; bytes 2 and 3 unknown |
| EEPROM check always answers 1 | approximated |
| Flash is 512 KiB with the layout the install tool writes; a new flash is zero with header state 2 | inferred; factory contents unknown |
| Fill-level register: a level in 32-bit words at two-word resolution | inferred; the FIFO's depth is unknown (at least 0x70c); the graphics flow-control credits are a different counter, doublewords, not modelled; "full while a list runs" is approximated |
| Interrupt-enable register: stores its twelve bits, reads them back, reset 0 | software never reads it (established); reset value unknown |
| Error register (+0x6c) reads 0 and ignores writes; no widget errors are raised | the register is write-one-to-clear and a healthy pipe reads 0 (inferred); power-on value unknown |
| A command-page store at +0xc is a plain word, like offset 0 | unknown; software only uses it when its CPU inventory is unrecognised, which an Onyx2 guest's is not (established) |
| The Onyx2 pipe's name is KONAL; the kernel supplies it, the board supplies nothing | established |
| Reasons post one at a time; a waiting one is looked for when the cause register is accessed and at the end of each run | approximated (QEMU polls on a timer) |
| Replies post at once instead of after queued work | approximated |
| A forced bracket with no context current suspends a user command with no owner to resume it | approximated (as QEMU) |
| The indirect data port does not advance its address on read | unknown |
| Retrace requests complete in `NullBackend` immediately | approximated |
| Identify and config part numbers, names, revisions and chip words | inferred from QEMU's model of a deskside pipe |
