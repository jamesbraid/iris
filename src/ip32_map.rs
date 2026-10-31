//! The O2's physical address map, and the switch for its per-register
//! access counters. The MACE and CRIME devices name these; keeping them out
//! of the bus decoder (ip32.rs) means a device does not depend on the bus
//! it sits on. ip32.rs re-exports all of it.

// ── IP32 physical map ───────────────────────
/// RAM base.
///
/// **Not zero.** The PROM's SizeMEM writes its probe patterns at `base + 0`,
/// `+0x01fffff8`, `+0x02000000` and `+0x07fffff8`, and the physical address
/// those land on is 0x40000000 — visible as the target of the `sd` at
/// 0xbfc05c9c. This is the "RAM at a non-zero base address" the O2 is known
/// for, and it collides with what NetBSD's `macereg.h` calls `MACE_PCI_NATIVE_VIEW`;
/// main memory wins that address, so the PCI window is not mapped here.
pub const RAM_BASE: u32 = 0x4000_0000;
/// CRIME: memory controller, interrupt controller and video DMA.
pub const CRIME_BASE: u32 = 0x1400_0000;
pub const CRIME_SIZE: u32 = 0x0000_1000;
/// GBE, the graphics back end, which scans the frame buffer out of RAM
/// (`crate::dev::o2gfx::Gbe`).
pub const GBE_BASE: u32 = 0x1600_0000;
pub const GBE_SIZE: u32 = 0x0010_0000;
/// VICE, the video and image compression engine.
pub const VICE_BASE: u32 = 0x1700_0000;
pub const VICE_SIZE: u32 = 0x0100_0000;

/// MACE, the I/O ASIC (`crate::dev::mace`): PCI, Ethernet, the ISA block.
pub const MACE_BASE: u32 = 0x1f00_0000;
pub const MACE_SIZE: u32 = 0x0080_0000;
/// MACE's ISA-block register holding the flash write-enable, the Dallas
/// identity line and the front LEDs (`crate::dev::mace_id`).
pub const MACE_ISA_FLASH_NIC_REG: u32 = MACE_BASE + 0x0031_0008;

/// `com0`, the PROM's console UART. NetBSD attaches it at `mace0 offset
/// 0x390000`; `com1` follows at 0x398000.
/// The Ethernet, MAC-110 (`crate::dev::mec`), and its CRIME interrupt.
pub const MACE_MEC: u32 = MACE_BASE + 0x0028_0000;
pub const MACE_MEC_SIZE: u32 = 0x0000_0200;
pub const IP32_MEC_CRIME_INT: u32 = 3;
pub const MACE_COM0: u32 = MACE_BASE + 0x0039_0000;
pub const MACE_COM1: u32 = MACE_BASE + 0x0039_8000;
pub const MACE_COM_SIZE: u32 = 0x0000_0800;

/// MACE's free-running timer (`MACE_UST_MSC`, i.e. `MACE_PERIF + 0x40000`).
/// The PROM busy-waits on this; a constant hangs it forever.
/// The two PS/2 ports, keyboard at +0x00 and mouse at +0x20, each
/// `tx / rx / control / status` at eight-byte spacing.
pub const MACE_PS2: u32 = MACE_BASE + 0x0032_0000;
pub const MACE_PS2_SIZE: u32 = 0x40;

pub mod ps2_bit {
    pub const CLOCK_SIGNAL: u64 = 1 << 0;
    pub const CLOCK_INHIBIT: u64 = 1 << 1;
    pub const TX_INPROGRESS: u64 = 1 << 2;
    /// The transmit holding register has drained. The PROM spins on this for
    /// up to 20834 tries after every byte it sends, so a port that never sets
    /// it is not merely idle — it is expensive.
    pub const TX_EMPTY: u64 = 1 << 3;
    /// A byte has arrived. Never set here: nothing is plugged in.
    pub const RX_FULL: u64 = 1 << 4;
    pub const RX_INPROGRESS: u64 = 1 << 5;
}

pub const MACE_UST_MSC: u32 = MACE_BASE + 0x0034_0000;
pub const MACE_UST_MSC_SIZE: u32 = 0x0001_0000;

/// MACE's PCI host bridge register block, at MACE + 0x080000.
pub const MACEPCI_BASE: u32 = MACE_BASE + 0x0008_0000;
pub const MACEPCI_SIZE: u32 = 0x0001_0000;

/// CRIME's rendering-engine block (`crate::dev::o2gfx::CrimeRe`). NetBSD's
/// `crmfb` maps exactly `0x15000000, 0x6000` for this.
pub const CRIME_RE_BASE: u32 = 0x1500_0000;
pub const CRIME_RE_SIZE: u32 = 0x0000_6000;

/// Low memory aliases the base of RAM.
///
/// POST writes a walking pattern to physical 0x00..0x20 from 0xbfc051b4, long
/// after it has sized memory at [`RAM_BASE`]. Without an alias those are
/// unmapped, the store takes a data bus error, and the PROM spins in its
/// exception vector. The Indy models the same idea (`ALIAS_BASE` in
/// `physical.rs`), so the shape is familiar even though the base differs.
///
/// The alias is the first 256 MB of the window ([`LOW_ALIAS_SIZE`]); memory
/// above that is only at its own address. The lowest device sits at
/// 0x14000000, so the alias cannot shadow one.

/// The memory window CRIME decodes: 1 GB from [`RAM_BASE`], which its eight
/// bank controls place banks of 32 or 128 MB in. How, and what an absent bank
/// answers, is in [`crate::dev::crime_mem`].



pub const RAM_WINDOW_SIZE: u32 = 8 * 128 * 1024 * 1024;
/// CRIME's no-ECC alias of the memory window: the same memory as
/// [`RAM_BASE`], read without ECC checking, at CPU physical 0x00_8000_0000.
/// IRIX maps pages it reads uncached here (`/dev/mem`, `/dev/kmem`). The bus
/// sees physical addresses as 32 bits and has MACE's 64-bit PCI window at
/// 0x8000_0000 ([`PCI_HI_MEM_BASE`]), so the CPU folds this one onto
/// `RAM_BASE` before the bus sees it (`MipsExecutor::phys_alias`).
pub const NO_ECC_BASE: u64 = 0x8000_0000;
/// How much of memory also answers from address 0: the first 256 MB.
pub const LOW_ALIAS_SIZE: u32 = 0x1000_0000;

/// Boot PROM. Same address as the Indy's, which is the one thing that carries
/// over: the MIPS reset vector is 0xbfc00000 on both.
pub const PROM_BASE: u32 = 0x1fc0_0000;
pub const PROM_SIZE: u32 = 0x0008_0000; // 512 KiB

/// Where the `firmware` section's SHDR says it loads: KSEG0 0x81000000.
pub const FIRMWARE_LOAD_VA: u32 = 0x8100_0000;
pub const FIRMWARE_SPAN: u32 = 0x0006_0000;

/// The RTC and its battery-backed NVRAM, at MACE + 0x3a0000. Registers use
/// MACE's ISA spacing, `(reg << 8) + 7`, exactly like the UART.
pub const MACE_RTC: u32 = MACE_BASE + 0x003a_0000;
pub const MACE_RTC_SIZE: u32 = 0x0001_0000;

/// Whether the per-register access counters (MACE's, GBE's, the rendering
/// engine's) run: `IRIS_IP32_HOT`. They are for finding a driver polling a
/// bit nothing sets, and each access took a lock and a map update, every
/// status poll of a GL scene included.
pub fn hot_counters() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("IRIS_IP32_HOT").is_some())
}
