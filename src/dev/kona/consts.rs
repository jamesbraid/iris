//! Offsets, field layouts and command numbers of the board's host interface,
//! all relative to the widget's 16 MiB aperture.
//!
//! Ported from include/hw/misc/sgi-kona.h and include/hw/misc/sgi-kona-ports.h
//! in jamesbraid/qemu-sgi-origami. Register names describe the role host
//! software gives the register. Statements about what host software does are
//! software expectations taken from QEMU's model, not hardware facts; the
//! grade of each (inferred, approximated, unknown) is in the module that acts
//! on it.

/// One widget's worth of address space; the rest reads unmapped.
pub const APERTURE: u64 = 16 << 20;

// Crosstalk widget header.
pub(super) const WIDGET_ID: u32 = 0x04;
pub(super) const WIDGET_CONTROL: u32 = 0x24;
pub(super) const INT_DEST_UPPER: u32 = 0x34;
pub(super) const INT_DEST_LOWER: u32 = 0x3c;
pub(super) const INT_VECTORS: u32 = 0x5c;
pub(super) const PROBE: u32 = 0x64;
pub(super) const LOW_WINDOW_WRITE: u32 = 0x7c;
pub(super) const ONEWIRE: u32 = 0x98;
pub(super) const NIC: u32 = 0x9c;

// Register window at +0x800000.
pub(super) const CREDIT: u32 = 0x800208;
pub(super) const CREDIT_CONFIG: u32 = 0x800210;
pub(super) const RESET_ARG: u32 = 0x800220;
pub(super) const INTR_END: u32 = 0x800228;
pub(super) const INT_ENABLE: u32 = 0x800230;
pub(super) const INT_CAUSE: u32 = 0x800238;
pub(super) const MAILBOX: u32 = 0x800240;
pub(super) const RETRACE_TAG: u32 = 0x800248;
pub(super) const RESET_STROBE: u32 = 0x800300;
pub(super) const EEPROM_ENABLE: u32 = 0x800308;
pub(super) const LOADER_328: u32 = 0x800328;
pub(super) const ACTIVITY: u32 = 0x8003a8;
pub(super) const LOADER_3B0: u32 = 0x8003b0;
pub(super) const INDIRECT_SELECT: u32 = 0x8003c0;
pub(super) const INDIRECT_DATA: u32 = 0x8003d0;
pub(super) const PORT_ADDR: u32 = 0x8003e8;
pub(super) const PORT_DATA: u32 = 0x8003f0;
pub(super) const PORT_WRITE_NEXT: u32 = 0x8003f8;
/// The kernel FIFO command push port. Approximated as a register-block
/// relative address: the driver's own citation is +0x7c, which the
/// low-window register claims.
pub(super) const DMA_FIFO_PUSH: u32 = 0x80007c;

// Flow-controlled window and the command page.
pub(super) const DMA_FIFO_KICK: u32 = 0xc00108;
pub(super) const COMMAND_PAGE: u32 = 0xc10000;
pub(super) const COMMAND_PAGE_SIZE: u32 = 0x4000;
pub(super) const COMMAND_DATA_SLOT: u32 = 0xc;
pub(super) const COMMAND_SLOT_SHIFT: u32 = 5;
pub(super) const COMMAND_SLOT_MASK: u32 = 0x1f;
pub(super) const COMMAND_TOKEN_MASK: u32 = 0x1ff;

// XG map RAM.
pub(super) const MAPRAM_BASE: u32 = 0x100000;
pub(super) const MAPRAM_ENTRIES: usize = (1 << 20) / 4;
pub(super) const PROBE_MAPRAM_COUNT_SHIFT: u32 = 3;
pub(super) const PROBE_MAPRAM_COUNT_MASK: u32 = 0x7 << PROBE_MAPRAM_COUNT_SHIFT;

// The widget id register.
pub(super) const WIDGET_ID_REVISION_SHIFT: u32 = 28;
pub(super) const WIDGET_ID_PART_SHIFT: u32 = 12;
pub(super) const WIDGET_ID_MANUFACTURER_SHIFT: u32 = 1;
pub(super) const WIDGET_ID_ALWAYS_ONE: u32 = 1;
pub(super) const PART: u32 = 0xc102;
pub(super) const MANUFACTURER: u32 = 0x2aa;

// The one-wire register, in the layout the Bridge and IOC3 share: a write
// starts a slot.
pub(super) const ONEWIRE_LEVEL: u32 = 1 << 0;
pub(super) const ONEWIRE_DONE: u32 = 1 << 1;
pub(super) const ONEWIRE_SAMPLE_MASK: u32 = 0xff << 2;
pub(super) const ONEWIRE_PULSE_MASK: u32 = 0x3ff << 10;
pub(super) const ONEWIRE_WRITABLE: u32 = ONEWIRE_SAMPLE_MASK | ONEWIRE_PULSE_MASK;
/// The Dallas family code of the board's identity part (a DS2502).
pub(super) const ONEWIRE_FAMILY: u8 = 0x09;

// Interrupt destination and vectors.
pub(super) const INT_DEST_WIDGET_SHIFT: u32 = 16;
pub(super) const INT_DEST_WIDGET_MASK: u32 = 0xf;
pub(super) const INT_VECTORS_GE_SHIFT: u32 = 9;
pub(super) const INT_VECTORS_GENERAL_MASK: u32 = 0xff;

/// The fill-level register's field, bits 10:1 on the second host-interface
/// revision: a level in 32-bit words at two-word resolution (inferred).
pub(super) const CREDIT_MASK: u32 = 0x7fe;
/// The interrupt-enable register's bits on the second revision (the first
/// has 0x3ff): one per status bit.
pub(super) const INT_ENABLE_MASK: u32 = 0xfff;

// Interrupt cause bits.
pub(super) const CAUSE_LOWATER: u32 = 1 << 1;
pub(super) const CAUSE_VIDEO: u32 = 1 << 3;
pub(super) const CAUSE_MAILBOX: u32 = 1 << 4;
pub(super) const CAUSE_GE: u32 = 1 << 5;

pub(super) const ACTIVITY_BUS: u32 = 1 << 2;

// The indirect port's internal address space.
pub(super) const FLASH_BASE: u32 = 0x800000;
pub(super) const ICU_CONTROL: u32 = 0x200050;
pub(super) const ARM_LOW_BASE: u32 = 0x0;
pub(super) const ARM_LOW_WORDS: usize = 0x2c / 4;
pub(super) const ARM_HIGH_BASE: u32 = 0x400_0000;
pub(super) const ARM_HIGH_WORDS: usize = (0x414_e630 - 0x400_0000) / 4;
/// Sections of the firmware load the board records.
pub(super) const LOAD_SECTIONS_MAX: usize = 32;

// Selectors the driver's attach reads its pipe class from through the
// indirect pair.
pub(super) const SELECT_PRODUCT: u32 = 0x8800;
pub(super) const SELECT_REDUCED: u32 = 0xb00;

// Bracket register bits.
pub(super) const BRACKET_FORCE: u32 = 0x20000;
pub(super) const BRACKET_PRIVILEGED: u32 = 0x10000;

// Page words: bit 28 marks a host physical page, otherwise the page word is
// a device address through the map RAM.
pub(super) const PAGE_WORD_PHYSICAL: u32 = 1 << 28;
pub(super) const PAGE_WORD_FRAME: u32 = 0x0fff_ffff;
pub(super) const MAPRAM_KIND_MASK: u32 = 0xf000_0000;
pub(super) const MAPRAM_KIND_16K: u32 = 0x2000_0000;
pub(super) const MAPRAM_PAGE_SHIFT: u32 = 14;

/// The textport's sync word.
pub(super) const TEXTPORT_SYNC_WORD: u32 = 1;
/// The textport command that opens a bitmap blit.
pub(super) const BITMAP_SELECTOR: u32 = 0xa;
pub(super) const BITMAP_HEADER_WORDS: usize = 6;

// Command page tokens the front end acts on.
pub(super) const CMD_REASON_PAGE: u32 = 0x1f1;
pub(super) const CMD_RETURN_PAGE: u32 = 0x1f2;
pub(super) const CMD_CONTEXT_MAP: u32 = 0x1fd;
pub(super) const CMD_POST_REASON: u32 = 0x1f8;
pub(super) const CMD_CHECK_PIPE: u32 = 0x1dd;
pub(super) const CMD_GET_CONFIG: u32 = 0x1e9;
pub(super) const CMD_EEPROM: u32 = 0x1ec;
pub(super) const CMD_CONTEXT_DEACTIVATE: u32 = 0x1fc;
pub(super) const CMD_CONTEXT_RESTORE: u32 = 0x1f9;
pub(super) const CMD_MEM_REGION: u32 = 0x1e4;
pub(super) const CMD_MEM_SWAP_OUT: u32 = 0x1e3;
pub(super) const CMD_MEM_SWAP_IN: u32 = 0x1e2;
pub(super) const CMD_MEM_SUSPEND: u32 = 0x1df;
pub(super) const CMD_RETRACE: u32 = 0x1e7;
pub(super) const CMD_X_INIT: u32 = 0x128;
pub(super) const CMD_X_COLOR_MAP: u32 = 0x12e;
pub(super) const CMD_X_GAMMA: u32 = 0x12f;
pub(super) const CMD_X_CURSOR: u32 = 0x130;
pub(super) const CMD_X_CURSOR_RECOLOR: u32 = 0x131;
pub(super) const CMD_VIDEO: u32 = 0x1a6;
pub(super) const CMD_DMA_PACKET: u32 = 0x1eb;
pub(super) const CMD_DMA_LIST_WRITE: u32 = 0x1c8;
pub(super) const CMD_DMA_POOL: u32 = 0x1ed;
pub(super) const CMD_LIST_CALL: u32 = 0x16d;
pub(super) const CMD_LIST_CALL_POOL: u32 = 0x16e;
pub(super) const CMD_LIST_CALLS: u32 = 0x170;
pub(super) const CMD_LIST_CALLS_POOL: u32 = 0x171;
pub(super) const CMD_LIST_PULL: u32 = 0x167;
pub(super) const CMD_LIST_MOVE: u32 = 0x16f;
pub(super) const CMD_GL_BUFFER_CONFIG: u32 = 0x1;
pub(super) const CMD_GL_TEX_TABLE: u32 = 0x17e;

/// The reasons the board posts on the reason page.
pub(super) const REASON_CONTEXT_DEACTIVATED: u32 = 0x101;
pub(super) const REASON_SWAP: u32 = 0x102;
pub(super) const REASON_SUSPEND: u32 = 0x104;
/// The mailbox word a DMA packet's completion posts, which the packet asks
/// for with its last word.
pub(super) const DMA_DONE_WORD: u32 = 0x100;

/// Words the GL texture table command carries after its slot word.
pub(super) const GL_TEX_TABLE_WORDS: u32 = 33;
/// Data words behind a cursor load: the image and the colours.
pub(super) const X_CURSOR_DATA_WORDS: u32 = 260;
/// Data words behind a cursor recolour.
pub(super) const X_CURSOR_RECOLOUR_WORDS: u32 = 4;

/// The words a context's save area holds after a deactivate: word 0 is the
/// model's own marker ("KONA"), word 2 a no-op token the kernel sends back.
pub(super) const SAVE_AREA_MARKER: u32 = 0x4b4f4e41;
pub(super) const SAVE_AREA_TOKEN: u32 = 0x172;

/// The sync probe's answer.
pub(super) const SYNC_PROBE_REPLY: u32 = 0x1234abcd;

/// Mailbox reply words the board queues at once.
pub(super) const MAILBOX_MAX_WORDS: usize = 5;
/// Completion reasons that may wait behind an unread one.
pub(super) const REASON_QUEUE: usize = 16;
/// The most words in one reason.
pub(super) const REASON_WORDS: usize = 2;

/// Display-list memory.
pub(super) const LIST_DEFAULT: usize = 15 << 20;
pub(super) const LIST_GRANULE: u32 = 0x400;
/// The most words a list invocation fetches: the display-list class's size.
pub(super) const LIST_MAX_WORDS: u32 = (LIST_DEFAULT / 4) as u32;
pub(super) const LIST_CALLS_MAX: u32 = 8;
pub(super) const LIST_CALLS_POOL_MAX: u32 = 4;
pub(super) const DMA_ADDR_TAG: u32 = 0x9000;
pub(super) const DMA_PAGE_SIZE: u64 = 4096;
pub(super) const DMA_POOL_PAGE: u32 = 0x4000;
pub(super) const DMA_POOL_MAX: u32 = 256 << 20;
/// The most a swap moves, a model bound.
pub(super) const SWAP_MAX_BYTES: u64 = 256 << 20;
