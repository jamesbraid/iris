# sgi-gr2: where each fact comes from

iris wrote its GR2 with the vendor's register headers as its stated reference, so no hardware fact in
the mounted files counts as grounded until a published document, a public driver or a shipped binary
states it. This note records, for the engine and the board file, what was dropped or replaced for that
reason, and what stays and on what support. It carries no vendor text.

Sources, by the gr2 survey's tags (the gr2 reverse-engineering repository's `origami-re` branch, commits
`f95a965`, `ad3ff54`, `62eb003` and `f1af046`; pages `platforms/express/host-contract.md`,
`devices/*/registers.toml`, `questions/*.md`). "Survey request" names below are that survey's list of what
must still be settled, spelled out in the last table:

- **[public]** NetBSD's `grtwo` driver, the Elan Graphics Technical Report, the Brooktree datasheets, Usenet
  `gfxinfo` and `hinv` postings.
- **[binary]** the IRIX IP20 kernel's graphics objects, the IP20 PROMs 4.0.5D and 4.0.5G, the diagnostic
  program, the X server driver and the GL core. A software expectation, not a hardware measurement.
- **approximation** iris's own policy, found by running guests on iris, with the symptom named.
- **header-only** nothing but the vendor headers states it; kept in iris's files until the survey grounds
  it, and listed at the end.

Ratings are Origami's six (established, observed, software expectation, inferred, approximation,
unknown). The survey says "established" for a fact it read in full in a shipped binary; for the record
here that is a *software expectation* (a program's expectation, not a hardware measurement), and
"established" is kept for what a datasheet or the hardware's own documentation states. Each row below uses
the six words; the survey's tag in brackets says where the fact came from.

## Dropped or replaced

| What | Was | Now | Why |
|---|---|---|---|
| Window at +0x6c1c0 to +0x6c1fc | iris decoded one word at +0x6c1c0 that read back its store, and zeros after, on every board | `Gr2Probe.aux_window`: `None` (an Indigo: unmapped, stores logged, reads 0), `Legacy` (iris's Xz and Extreme: exactly as before), `Bytes` (sixteen byte cells that hold their last byte; off for iris) | The IP20 kernel, PROMs, X driver and GL core never touch the range (survey question on the +0x6c1c0 range, `questions/re3.md`). The IP22 kernel's flat-panel interface object does (survey commit f1af046, [binary]): its probe stores 0, 0x55 and 0xaa at +0x6c1e0 + idx and reads each back expecting the same byte, with byte routines at +0x6c1c0, +0x6c1c4 and +0x6c1e0 + idx. *Software expectation*: a byte there holds its value, no richer. `Bytes` is off for iris because a probe that stores and reads back sees a device the board is not configured with: iris's old zero-reading CC1 made that probe fail, and iris reports no panel (its port 1 corona bits are header-only, below). Nothing says what the compare decides, and no IP22 guest was run. Turn it on only after a run shows the guest still boots and reaches X. |
| Board revision, monitor code and fitted-board bytes of the four configuration ports | constants of the engine for iris's Xz (revision 4) and Extreme (6) | `Gr2Probe.ports`, given by the shell; iris's values live in `variant.rs` | The decoding is [binary] (`Gr2Probe`, PROM probe), established; the values belong to the machine. An Indigo reports revision 2 [public], port 0 byte 0xdd, one engine for XS and XS24, two for XZ, four for Elan (inferred from the decode and the postings). |
| HQ2 revision in the status word and in `hq_gepc` | constant 2 | `Gr2Probe.hq_rev` | The Indigo's HQ2 revision answers 0 in the survey's table (inferred); iris keeps 2, and its test needs it non-zero. |
| Geometry engine count | `Gr2Variant::ges` in the engine | `Gr2Probe.ges` | As the ports. XS and XS24 one, XZ two, Elan four, iris's Extreme eight; absent slots answer zero without a bus error [binary]. |
| Host clock in the two stall limits | `Instant` | the shell's time (`Gr2::set_time`) | Not a hardware fact: a paused machine must not time a guest out. |

## Kept, with their support

| Constants | Tag | Support |
|---|---|---|
| Slot window layout: shared RAM at 0, FIFO window +0x40000, microcode RAM +0x60000, HQ2 registers +0x6a000, finish-3 port +0x6b000, board ports +0x6c000, clock data +0x6c020, VC1 +0x6c040, DACs +0x6c0a0 (stride 0x20), XMAPs +0x6c100 (stride 0x20), broadcast +0x6c1a0, GE windows +0x68000 | software expectation ([binary]) | `devices/{hq2,ge7,clock,vc1-express,xmap5,bt457}/registers.toml`. The first GE window (`slot0_ram`) is inferred; the DAC and XMAP instance strides and the slot size are not in the tables read. |
| HQ2 registers: status, numge, fin1, fin2, dmasync, fifo_full_timeout, fifo_empty_timeout, fifo_full, fifo_empty, ge7loaducode, gedma, gepc, unstall, probe word 0xdeadbeef, refresh | software expectation ([binary]) | `devices/hq2/registers.toml` |
| `hq_gepc` (+0x6a06c) and `intr` (+0x6a074) | inferred ([binary]) | as above |
| NetBSD's finish-3 offset +0x6a100 | unknown ([public] NetBSD only) | Kept: iris answers it. No shipped program uses it; the binaries use +0x6b000. |
| Finish flags in status bits 0, 1, 2; `fin1`/`fin2` clear ports; FIN2 raised by pixel-DMA completion and context save | software expectation ([binary]) | host-contract section 1 and 3 |
| Shared-RAM word 0x7fff = "textport microcode running" (`TP_PROBE_ID`) | software expectation ([binary]) | `ge7` `ucode_ready` at +0x1fffc |
| GE7 load and read-back path: staging registers, `ge7loaducode` mask 0x03df_ffff, revision byte at the GE window's word 0xfd (bits 7:5, iris reports 1 = rev B) | software expectation ([binary]; the mask was corrected in the survey review) | `devices/ge7/registers.toml` |
| VC1 ports (command, sram, testreg, addrlo, addrhi, sysctl) and revision 1 | software expectation ([binary]) | `devices/vc1-express/registers.toml`; `xmapmode` (+0x6c044) is unknown and iris stores a byte there |
| XMAP5 ports (mode, clut, addrlo, addrhi, fifostatus), five instances and the broadcast port | software expectation ([binary]) | `devices/xmap5`; `crc` and `bytecount` unknown, `misc` inferred |
| Bt457 ports and the 256-entry palette, read mask | established ([public] datasheet) | `devices/bt457` |
| Pixel clock load on the data port: 17-byte (revision below 4) and 56-bit (revision 4 and up) sequences, recorded only | software expectation ([binary]) | `devices/clock/README.md`; the part and the frequencies are unknown, so the clock is not modelled |
| Read port and data port are one address, HQ2_GEDMA (+0x6a068) | software expectation ([binary]) | host-contract section 3 |
| Picture size: width from VC1 SRAM half-word 0x9f2 (a 1024-wide mode is stored as 1025), height from registers 0x40 and 0x42 (2 bytes a line), else the monitor code: 1024 by 768 for code 6, 1280 by 1024 otherwise | software expectation ([binary]) for the stores and the code decode; the size from the code is inferred | `devices/vc1-express/README.md` (init steps, timing files), host-contract section 4, `platforms/express/display.md`. A mode smaller than 1280 by 1024 shows the top left corner of iris's composition; where the hardware places it is unknown (approximation). |
| Frame rate in `head_timing`: 72 Hz for monitor codes 1 and 9, 60 Hz for the others except 6 and 14, only while a built-in timing is loaded (VC1 register 0x14 clear, progressive) | inferred | The built-in sets are byte-identical to the shipped `72hz.u` and `60hz.u` (survey, confirmed) and the file names and format ids give the rates. A loaded file, an interlaced mode and codes 6 and 14 give no timing and the shell keeps its own. |
| `HeadTiming`'s clock: a 3.6 MHz virtual clock (50,000 clocks a frame at 72 Hz, 60,000 at 60), `lines` the visible height, `epoch_ns` -1 | approximation | No pixel clock, line total or blanking is established, and the model has no blanking. A shell that derives a line time from `lines` gets a wrong one; only the frame period is meant. The test `the_size_and_timing_follow_the_vc1_and_the_monitor_code` checks the decode and the rates, not the approximation; a discriminating test needs the survey's line program decoded. |
| Monitor code 6 gives 1024 by 768 at any board revision | inferred | The survey states it for revisions below 4 and elsewhere only "code 6"; harmless for iris (code 0). It matters only before the VC1 is programmed, and the shell sees the size change when the mode registers are written. |
| FIFO window depth 65,536 entries, RE3 queue 65,536, read-port ring 16,384 words | approximation | The hardware FIFO is 64 words [binary]. iris's is deep so a producer never stalls on it; each finish flag is kept honest by the pending-finish counter (clanker/gr2/fin3-must-track-pending-finish.md). |
| FIN2 poll stall and read-port stall limits, 2 s each, in shell time | approximation | Found against IRIX 5.3 and 6.5.22 on iris: the kernel's 100,000 x 1 us poll gives up before a large pixel DMA finishes (clanker/gr2/fin2-wait-must-stall.md). |
| No interrupt line: `irq_levels` is 0 | approximation | The FIFO level drives GIO line 0 (software expectation, [binary]; the level rule inferred). A model that executes no microcode has no fill count to compare, so the PROM's wait for the line to clear never waits. |

## Kept with only the headers behind them

These stay in iris's mounted files until the survey answers the request named. None is in the engine
file, the state file or the C interface.

| Where | What | Survey request that settles it |
|---|---|---|
| `hq2.rs` | Operand layouts of the console tokens; the kernel's context-save and swap tokens and the shared-RAM words they read back; the X server's token names and layouts (the slot numbers come from the X driver) | context switch and swap; token catalogue |
| `gl.rs`, `gl_light.rs` | The token-address modifier bits; the window-clip token's layout; the colour fixed-point format; "blending runs in microcode, no alpha test" | token catalogue |
| `re3.rs`, `re3_jit/` | The register layout and meaning. No shipped program touches +0x6c200 to +0x6c7ff [binary, negative search], so RE3 is an internal interface under iris's names; the window stays for iris's tests and the shell should not map it. Pixel rules (coverage, iterator precision, dither) are approximations. | display composition; RE3 exposure |
| `vc1.rs` | Display-ID table format, cursor encoding and its timing-table offsets, control bits | VC1 |
| `xmap5.rs` | Mode-word fields, colour-map layout and size, cursor colour entries | XMAP5 |
| `gr2comp.rs` | Plane priority and the per-pixel plane layout | display composition |
| `hq2.rs` | Status bits other than those above; FIFO count and the interrupt level | HQ2 status and interrupts |
| `variant.rs`, `Gr2Probe` | iris's board revisions 4 (Xz) and 6 (Extreme), eight engines on the Extreme, HQ2 revision 2, monitor code 0, and port 1 as `0x3e`, whose bits 3:2 are iris's old "no corona" reading, which the survey does not decode; ports 2 and 3 as absent | iris variants (survey request 13: the IP22 and Indy boards) |
| `hq2.rs` | Status bit 13 (a FIFO low-water flag) set on every board, as iris always set it. No IP20 program reads it; the IP22 kernel is not covered by that finding | HQ2 status and interrupts |
| Comment scrub done for the QEMU copy | The comments that named a vendor header, a header section, a vendor source file or an `ignore/` path now say what they mean and cite none of them. The facts they carried are unchanged and still rest on the headers alone where this table says so. What remains are comments naming the shipped GL library and its FIFO layout, which are software expectations from shipped binaries, and iris notes under `clanker/`. | |

Not covered: the HQ2 and GE7 instruction sets (the interpreter is a high-level model, and GE7 microcode is
stored and read back, never run); the identity check of the loaded microcode against the survey's image
hashes (survey request: microcode identity) is not built.
