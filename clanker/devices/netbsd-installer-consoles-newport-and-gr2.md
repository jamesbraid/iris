# NetBSD's installer on iris: how to boot it, and why 11.1 and 8.0 show no text

Status: history (2026-11-21). Measured with NetBSD 11.1 and the 11.99.11 daily
sgimips ISOs, `--ci`, no window.

**Booting it headless.** On the Indy PROM menu: `ps2 enter` dismisses the
failed autoboot, the pointer moves twice what `ps2 mouse` asks, and the menu
takes a click on the icon, not the label. Command Monitor, then
`boot -f scsi(0)cdrom(4)rdisk(0)partition(8)/ip2xboot` (the volume header
lists `ip2xboot`). The kernel loads in ~2 minutes on a `loop` build. The
`[scsi.4]` entry needs `cdrom = true`.

**Fixed.** The installer ignored keys on the Indy (8042 translate bit, see
ps2-8042-translate-bit-must-translate.md) and panicked on `--cpu r5000`
(the R5000's Config line size bits were writable).

**11.1 on the Indy: text is drawn black on black, by the guest.** A REX3
trace of one redraw shows 11.1 writing COLORI = 0 for the glyphs where
11.99.11 writes 0xdb. Read from guest memory (kernel symbols from the ISO,
`mem` on `newport_console_screen`): the attr is the same in both (fg 7), and
11.1's `ri_devcmap[]` is all zero where 11.99.11's holds the RGB332 values.
Cause: the console's `ri_stride` is `dc_xres`, which newport_get_resolution
sums from the PROM-written VC2 timing table as 1282. rasops_init() returns -1
on a stride that is not a multiple of 4 before it fills `ri_devcmap`
(the 11.1 log prints "rasops_init: stride not aligned on 32-bit boundary"
twice). Trunk's newport.c (1.26, 2026-08-18) uses `roundup(dc_xres, 4)` and
says the VC2 tables do not guarantee a multiple of 4. iris only stores and
returns that table, and the PROM and IRIX write the same 1282 (see
cursor-drawn-3px-right-of-x-pointer.md). Not an iris bug.
The probe misreads (board revision 0, "cmap revision 0") are also NetBSD's: its
setup_hw reads DCBDATA0 after a VC2 access, and a REX3 DCBDATA read always
runs a fresh cycle with the current DCBMODE. They only pick the depth of the
failed cnattach init. Unchecked: a real Indy's table width.

**GR2 (Indigo2 with `board = "gr2"`): works once DRAWCHAR reads narrow rows
right.** NetBSD's grtwo console sends PUC_COLOR, PUC_CMOV2I and PUC_DRAWCHAR
with mode 2 for every glyph width and 16-bit rows (the byte twice). Rows in
that case are bits 15:0, bit 15 leftmost (OpenBSD's driver sends the same
shape), so `drawchar` in `src/dev/gr2/hq2.rs` takes xsize <= 16 as 16-bit rows
whatever the mode; the PROM's wide mode 2 glyphs stay 32-bit rows. The screen
stays black first, and that is the guest's own delay() calls, not drawing: the
console clear sends 2040 big PUC_RECTI2D fills (x1, y1, x2, y2 inclusive corners;
y1 steps by 32, y2 stays 1007) with `delay(100000)` after each, about 240 s of
requested delay in all. A 1281x721 fill costs 1.4-2 ms with the GR2 JIT and
3.9 ms without; all of them together are about 2-7 s. Time from the boot
command to text, one or two runs each on a loaded host: 15 min on a default
loop build, 8-10 min with `lightning,gpujit` (ship), about 1 min with
`lightning,gpujit,jitv2`, so the CPU engine is what shortens it. Then the
installer's language menu is readable. A reverse-video menu line shows only
its `>`: the driver draws no background. Unknown: what the real ucode does
with those RECTI2D rectangles, and whether NetBSD's CLUT writes (XMAP #4
address, broadcast data port) reach the other XMAPs on a real board; the
display uses the PROM's CLUT page, so it does not matter here.

**NetBSD 8.0 stops in its bootloader: the install kernel is too big for the
Indy's firmware, not an emulator bug.** The loader prints the kernel's file
size (`7701808`) and the PROM's exception handler then runs on a garbage stack.
`ip2x` has one load segment at 0x88060000, 0x758530 bytes, so it runs past
0x88740000, where the ARCS memory list puts a 768 KB FirmwareTemporary block.
The PROM keeps its file descriptor table (0xa8740dd0) and exception stack
(0xa8740e40) there; the kernel text overwrites them, and the loader's next
Read faults. The 11.99.11 kernel ends at 0x88737df0, below the block. NetBSD
shrank that kernel for the same reason in 2018 (INSTALL32_IP2x 1.10, PR
port-sgimips/53378: "the ARC BIOS on Indy seems to have ~8MB limit"). Use a
newer image or netboot 8.0. Not checked: a real R4400SC's L2 may delay the
corruption.
