//! SGI DMediaPro DM8 PCI audio (1412:1724 revision 1): the IC Ensemble
//! ICE1724 (Envy24HT) controller with the converter wiring of an M-Audio
//! Revolution 7.1 board, and an AC'97 variant of the same controller.
//!
//! BAR0 (32 bytes, I/O) is the controller block, "CCS"; BAR1 (128 bytes,
//! I/O) is the multi-channel block, "MT". Both are little-endian byte
//! register files: a wider access is byte steps in address order. Seven DMA
//! engines share one sample rate. The `dm8` variant has two write-only AKM
//! converters behind bit-banged GPIO pins and a configuration EEPROM on the
//! I2C port; the `ac97` variant has no converter pins and no EEPROM, and a
//! plain AC'97 register file behind MT04 to MT06.
//!
//! Where a fact is established, the code follows it. Where this file departs
//! from one, or fills an unknown with a safe behaviour, the comment says
//! "Approximation". An access to a register or value that nothing
//! establishes is logged (`dm8` in the monitor's `log` command), never
//! silently accepted.
//!
//! Approximations kept from the model:
//!
//! * Soft reset (CCS00 bit 7) is not described for this chip. It restores
//!   every register except CCS00 and CCS04 to CCS07, as the VT1720 sibling
//!   part does; the bit does not self-clear. The converters and a frame in
//!   flight are not touched, nor does PRST# (MT05 bit 7, which also drives
//!   it in I2S mode) reach any converter.
//! * The EEPROM image is synthesized from constraints alone: the subsystem
//!   pair, a 49.152 MHz crystal with four stereo DACs and an I2S converter,
//!   the converter pins as outputs, CCLK, the selects and the mute line
//!   high. Everything else is inert. The crystals are unknown, and so is
//!   whether the board sets "external S/PDIF out implemented" (CCS07 bit 0;
//!   the image keeps the reset value).
//! * An I2C transfer completes at once, where hardware takes about a third
//!   of a millisecond. A transfer to an address nothing answers is logged
//!   and leaves CCS12 alone; NAK handling is not described.
//! * An input GPIO pin reads its weak pull-up (1). What an input reads is
//!   "the pin"; board straps and pull-ups are not documented.
//! * Every complete converter frame is accepted and recorded; the parts
//!   ignore writes while their PDN pin is low, and how the board wires PDN
//!   is unknown. A frame of other than 16 bits, a read command, a wrong chip
//!   address or an undefined register is refused and counted. Analog
//!   behaviour (soft mute, attenuation, the mute line on GPIO 22, the rate
//!   fields) is not modelled: samples go to the host unchanged. Which parts
//!   the DM8 carries on which selects is inferred (board fit).
//! * CCS1C reserved bits 6 and 1 read 0 and ignore writes. Bit 0, the
//!   multi-channel I2S serial clock, stops every engine when set; that the
//!   engines stop is inferred from the clock being stopped.
//! * The AC'97 index decodes as `(MT04 & 0x7e) >> 1` into a 64-entry file;
//!   only "bits 6:0 read/write" is established. Cycles finish at once, there
//!   is one codec (ID 0), ready after the cold reset bit clears; its part,
//!   reset values and volume and rate registers are unknown. In I2S mode and
//!   on the `dm8` variant no codec answers and cycles read zero.
//! * Underrun and overrun (MT1A, MT00 bit 3) are set only by a failed bus
//!   transfer, never by pacing. A failed transfer is not described either:
//!   the engine keeps its clock, plays or records silence, the PCI status
//!   records a master abort, and MT1A flags the engine. A transfer also
//!   fails when bus mastering is off in the command register.
//! * The first record engine (RDMA0) records the host's default input as
//!   16-bit stereo in the top half of each sample, converted to the card's
//!   rate; the second (RDMA1, the S/PDIF record engine) records silence. The
//!   input opens on the first start of RDMA0 (`[audio] capture`), not
//!   before. With no input device, or the ring run dry, the engine records
//!   silence, and a failure to open is reported once. The input closes when
//!   the engine stops, and a stream that dies is opened again. Samples the
//!   host gathered while the engine was stopped are dropped when it starts,
//!   and no more than the request being read and one behind it are kept.
//!   Buffers are written and the position and interval run either way.
//! * MT3C: every field but the validity flag (bit 15) must be written only
//!   while the S/PDIF transmitter is disabled (CCS07 bit 7 clear). The model
//!   enforces that rule: bit 15 stays writable, the rest ignore writes and
//!   log. The register reads 0000 at reset, where the chip gives x000.
//! * A masked source does not latch into MT00. The chip's wording suggests
//!   this; it does not say.
//! * Terminal counts are write-only and read 0. A write to an address or
//!   count register restarts the buffer position, a write to a terminal
//!   count the interval, also while running. Counts reload and the interval
//!   repeats.
//! * MT01: the external master clock (bit 4) is ignored and the rate code
//!   sets the pace. The reserved code 1101 keeps the previous rate. The
//!   128x/256x ratio and the crystal do not change timing; an impossible
//!   192 kHz setting is logged.
//! * Output routing (MT2C) and the MPU-401 UART store what is written and
//!   log it; the peak meters read zero.
//! * Power management: D1 is refused, D2 and D3hot only close both BARs (an
//!   access to a closed BAR floats high), and D3hot to D0 resets the chip.
//!   Whether registers answer in D2 and D3hot is not described. The
//!   subsystem mask is at 0x42; the chip also shows it at 0x40. A
//!   configuration access to a register nothing describes (BARs 2 and 3,
//!   0x40, 0x41, 0x43, 0x60) reads 0 and ignores writes. The bridge merges
//!   byte and halfword accesses into a dword, so such an access is logged
//!   whenever its dword is read or carries a nonzero value there.
//! * The sample clock runs on host time, less the time the machine was
//!   stopped, not on the emulated CPU's progress. A jump of more than a
//!   second is not replayed through memory; the engine resumes at the
//!   present.
//! * Hold-up approximation, an emulator policy with no hardware
//!   counterpart: a timer more than one flow tick behind its deadline means
//!   the host held up the timer thread, so the card's clock is set back to
//!   the deadline (at most 200 ms in all) instead of replaying the time in
//!   one burst; on-time ticks win it back (see
//!   [`CardClock::absorb_stall`]). The sample clock falls behind wall time
//!   by the hold-ups.
//! * `dropped_frames` counts frames played into a host ring left with no
//!   room.
//! * The interleaved engine's slots, and the S/PDIF engine, play through one
//!   host stream with as many channels as the host device has (7.1, 5.1,
//!   quad or stereo). Which speaker a slot drives is not established; the
//!   model takes the usual host orders: 2 slots front left and right; 4
//!   adds the back pair; 6 is 5.1 (front, centre, low frequency, back); 8
//!   adds the side pair. A host with fewer speakers than the slots folds
//!   them: centre, back and side channels at -3 dB into the front pair, a
//!   side pair into the back pair of a host that has one, the low-frequency
//!   channel dropped, and a speaker that receives folds scaled to sum to
//!   unity. Extra host speakers stay silent. The S/PDIF pair adds, with
//!   saturation, to the front pair, frame for frame with the analog stream
//!   when both run. The stereo engines PDMA1 to PDMA3 are moved and
//!   counted, and heard nowhere.
//!
//! Ported from hw/audio/sgi-dm8.c in jamesbraid/qemu-sgi-origami.

use std::fmt;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::config::AudioConfig;
use crate::dev::host_pcm::{CardClock, PcmIn, PcmOut};
use crate::devlog::LogModule;
use crate::host_audio::FrameResampler;
use crate::hptimer::{TimerId, TimerManager, TimerReturn};
use crate::irq::IrqLine;
use crate::pci::{Bar, PciBusDevice, PciConfig, PciDma, PCI_INTA};
use crate::snapshot::{field, field_bool, field_u16, field_u32, field_u64, field_u8, field_u8s, load_u16_slice, StateTable};
use crate::traits::{Device, Saveable};

pub const PCI_VENDOR_ICE: u16 = 0x1412;
pub const PCI_DEVICE_ICE1724: u16 = 0x1724;
pub const DM8_REVISION: u8 = 1;
/// Multimedia, audio device, programming interface 0.
const PCI_CLASS_AUDIO: u32 = 0x04_01_00;
/// The subsystem ID the DM8's EEPROM supplies: inferred for the board.
const SUBSYSTEM_DM8: u16 = 0x3630;
/// The chip's default when no EEPROM supplies one.
const SUBSYSTEM_NO_EEPROM: u16 = 0x1724;

pub const CCS_SIZE: u32 = 0x20;
pub const MT_SIZE: u32 = 0x80;

const PCI_COMMAND_BUS_MASTER: u32 = 0x4;
/// PCI status: interrupt pending, capability list, medium DEVSEL.
const STATUS_INTX: u32 = 0x08;
const STATUS_BASE: u32 = 0x0210;
/// Parity, system error, received target and master abort: write one to
/// clear.
const STATUS_W1C: u32 = 0xf000;
const STATUS_MASTER_ABORT: u32 = 0x2000;
const PM_CAP_DWORD: usize = 0x80 / 4;
const PMCSR_DWORD: usize = PM_CAP_DWORD + 1;
/// Capability ID 1, end of list, version 1.0 with D2 and no D1 or PME.
const PM_CAP_HEADER: u32 = 0x0401_0001;
const SVID_MASK_DWORD: usize = 0x40 / 4;
const SVID_WRITE_ENABLE: u32 = 0x80 << 16;
const SUBSYSTEM_DWORD: usize = 0x2c / 4;

// Controller block registers (BAR0).
const CCS_CONTROL: u32 = 0x00;
const CCS_INT_MASK: u32 = 0x01;
const CCS_INT_STATUS: u32 = 0x02;
const CCS_SYS_CONFIG: u32 = 0x04;
const CCS_AC_CONFIG: u32 = 0x05;
const CCS_I2S_FEATURES: u32 = 0x06;
const CCS_SPDIF_CONFIG: u32 = 0x07;
const CCS_UART_TX_COUNT: u32 = 0x0a;
const CCS_UART_RX_COUNT: u32 = 0x0b;
const CCS_UART_DATA: u32 = 0x0c;
const CCS_UART_COMMAND: u32 = 0x0d;
const CCS_UART_SETTING: u32 = 0x0e;
const CCS_I2C_DEVICE: u32 = 0x10;
const CCS_I2C_BYTE: u32 = 0x11;
const CCS_I2C_DATA: u32 = 0x12;
const CCS_I2C_STATUS: u32 = 0x13;
const CCS_GPIO_DATA_LO: u32 = 0x14;
const CCS_GPIO_MASK_LO: u32 = 0x16;
const CCS_GPIO_DIR: u32 = 0x18;
const CCS_POWER_DOWN: u32 = 0x1c;
const CCS_GPIO_DATA_HI: u32 = 0x1e;
const CCS_GPIO_MASK_HI: u32 = 0x1f;

const CCS_INT_MPU_RX: u8 = 0x80;
const CCS_INT_MPU_TX: u8 = 0x20;
const CCS_INT_MT: u8 = 0x10;
const CCS_INT_STATUS_WRITABLE: u8 = 0xe0;
const CCS_CONTROL_SOFT_RESET: u8 = 0x80;
const CCS_AC_I2S: u8 = 0x80;
const CCS_SYS_CRYSTAL_49: u8 = 0x40;
const CCS_I2C_EEPROM: u8 = 0x80;
const CCS_PD_MT_CLOCK: u8 = 0x01;

// Multi-channel block registers (BAR1).
const MT_STATUS: u32 = 0x00;
const MT_RATE: u32 = 0x01;
const MT_FORMAT: u32 = 0x02;
const MT_MASK: u32 = 0x03;
const MT_AC97_INDEX: u32 = 0x04;
const MT_AC97_CMD: u32 = 0x05;
const MT_AC97_DATA: u32 = 0x06;
const MT_START: u32 = 0x18;
const MT_CHANNELS: u32 = 0x19;
const MT_FIFO_ERROR: u32 = 0x1a;
const MT_PAUSE: u32 = 0x1b;
const MT_ROUTING: u32 = 0x2c;
const MT_SPDIF_CTRL: u32 = 0x3c;
const MT_PEAK_INDEX: u32 = 0x3e;
const MT_PEAK_DATA: u32 = 0x3f;

const MT_AC97_COLD_RESET: u8 = 0x80;
const MT_AC97_WRITE: u8 = 0x20;
const MT_AC97_READ: u8 = 0x10;
const MT_AC97_READY: u8 = 0x08;
const MT_FORMAT_128X: u8 = 0x08;

// The Revolution 7.1 pin assignment.
const GPIO_CCLK: u32 = 1 << 1;
const GPIO_CDOUT: u32 = 1 << 3;
const GPIO_CS_FRONT: u32 = 1 << 5;
const GPIO_CS_SURROUND: u32 = 1 << 6;
const GPIO_MASK_ALL: u32 = 0x7f_ffff;

/// Engines are numbered by their bit in MT00, MT18, MT1A and MT1B. Bit 3 of
/// MT00 is the FIFO error source and has no engine.
const ENGINES: usize = 8;
const ENG_PDMAI: usize = 0;
const ENG_RDMA0: usize = 1;
const ENG_RDMA1: usize = 2;
const ENG_PDMA1: usize = 4;
const ENG_PDMA2: usize = 5;
const ENG_PDMA3: usize = 6;
const ENG_PDMA4: usize = 7;
const ENGINE_BITS: u8 = 0xf7;
const FIFO_ERROR_SOURCE: usize = 3;

/// A chunk is a whole number of frames for 2, 4, 6 and 8 slots.
const CHUNK_DWORDS: u32 = 3840;
/// Engines are brought up to date at least this often while they run.
const FLOW_NS: i64 = 2_000_000;
/// Most channels the host stream is opened with: the interleaved engine's
/// eight slots.
const MAX_HOST_CHANNELS: usize = 8;
const NS_PER_SEC: u128 = 1_000_000_000;
const MAX_CATCHUP_SECONDS: u64 = 1;

const EEPROM_SIZE: usize = 256;
const EEPROM_I2C_ADDRESS: u32 = 0x50;
const AC97_REGS: usize = 64;
const AK4381_REGS: usize = 5;
const AK4355_REGS: usize = 11;
const AK4381_CHIP_ADDRESS: u32 = 1;
const AK4355_CHIP_ADDRESS: u32 = 3;
/// Sample rates by MT01 bits 3:0; code 13 is not listed.
const RATES: [u32; 16] = [
    48000, 24000, 12000, 9600, 32000, 16000, 8000, 96000, 44100, 22050, 11025, 88200, 176400, 0, 192000, 64000,
];

/// The synthesized EEPROM image (see the module notes).
const EEPROM_IMAGE: [u8; 19] = [
    0x36, 0x30, 0x14, 0x12, // 0..3: subsystem pair
    0x13, 0x02, // 4: size, 5: version
    0x43, 0x80, 0x01, 0x01, // 6..9: CCS04 to CCS07
    0x7a, 0x00, 0x40, // 10..12: direction, GPIO 1, 3, 4, 5, 6, 22
    0x85, 0xff, 0xbf, // 13..15: write mask, those pins open
    0x72, 0x00, 0x40, // 16..18: data, lines idle, not muted
];

/// Which of the controller's two personalities this card has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Variant {
    /// The DMediaPro DM8: AKM converters behind GPIO pins, an EEPROM.
    Dm8,
    /// A board with an AC'97 codec: no converter pins, no EEPROM.
    Ac97,
}

impl Variant {
    fn eeprom_fitted(self) -> bool {
        self == Variant::Dm8
    }
}

/// A register of a DMA engine.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Addr,
    Count,
    Tc,
}

/// Register offsets of each engine's address, count and terminal count. The
/// count and terminal count of the interleaved engine are 19 bits in three
/// bytes; the stereo engines have 16 bits in two. Engine 3 has none.
const ENGINE_REGS: [(u32, u32, u32); ENGINES] = [
    (0x10, 0x14, 0x1c),
    (0x20, 0x24, 0x26),
    (0x30, 0x34, 0x36),
    (0, 0, 0),
    (0x70, 0x74, 0x76),
    (0x60, 0x64, 0x66),
    (0x50, 0x54, 0x56),
    (0x40, 0x44, 0x46),
];

/// The engine, register and byte an MT offset falls on.
fn engine_decode(off: u32) -> Option<(usize, Field, u32)> {
    for (i, &(addr, count, tc)) in ENGINE_REGS.iter().enumerate() {
        if addr == 0 {
            continue;
        }
        let wide = if i == ENG_PDMAI { 3 } else { 2 };
        if (addr..addr + 4).contains(&off) {
            return Some((i, Field::Addr, off - addr));
        }
        if (count..count + wide).contains(&off) {
            return Some((i, Field::Count, off - count));
        }
        if (tc..tc + wide).contains(&off) {
            return Some((i, Field::Tc, off - tc));
        }
    }
    None
}

#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct Engine {
    /// Buffer base, bits 31:2.
    addr: u32,
    /// Buffer size in dwords, minus one.
    count: u32,
    /// Interrupt interval in dwords, minus one.
    tc: u32,
    /// Dwords moved in this pass of the buffer.
    pos: u32,
    /// Dwords moved in this interval.
    tc_pos: u32,
    /// Frames moved since the clock anchor.
    frames: u64,
    /// The card clock when the frame count started, in nanoseconds.
    anchor: i64,
    /// The clock is running for this engine.
    active: bool,
}

impl Engine {
    fn field(&mut self, f: Field) -> &mut u32 {
        match f {
            Field::Addr => &mut self.addr,
            Field::Count => &mut self.count,
            Field::Tc => &mut self.tc,
        }
    }
}

/// What the converters and the engines have done, for tests and for
/// comparing against a guest driver's intent.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Stats {
    /// Frames the interleaved engine moved.
    pub analog_frames: u64,
    /// The last dword of the last analog chunk moved.
    pub analog_last: u32,
    pub spdif_frames: u64,
    pub capture_frames: u64,
    /// Frames the host stream had no room for.
    pub dropped_frames: u64,
    pub ak4381_writes: u32,
    pub ak4355_writes: u32,
    /// Converter frames refused.
    pub codec_rejects: u32,
}

struct State {
    ccs: [u8; CCS_SIZE as usize],
    mt: [u8; MT_SIZE as usize],
    pm_state: u8,
    /// PCI status bits 15:12, which latch until written one.
    status_errors: u16,
    irq_level: bool,
    eeprom: [u8; EEPROM_SIZE],
    eng: [Engine; ENGINES],
    rate: u32,
    /// The level on each converter pin after the last GPIO change.
    gpio_pins: u32,
    frame_bits: [u8; 2],
    frame_word: [u16; 2],
    ak4381: [u8; AK4381_REGS],
    ak4355: [u8; AK4355_REGS],
    ac97: [u16; AC97_REGS],
    stats: Stats,
    /// The timer that next brings the engines up to date, and which of
    /// them is current.
    timer: Option<TimerId>,
    timer_gen: u64,
    /// When the pending timer is due, on the card's clock.
    timer_due: i64,
    /// Samples read from or written to guest memory, a chunk at a time.
    scratch: Vec<u8>,
}

impl State {
    fn new(variant: Variant) -> Self {
        let mut eeprom = [0xff; EEPROM_SIZE];
        if variant.eeprom_fitted() {
            eeprom[..EEPROM_IMAGE.len()].copy_from_slice(&EEPROM_IMAGE);
        }
        let mut s = Self {
            ccs: [0; CCS_SIZE as usize],
            mt: [0; MT_SIZE as usize],
            pm_state: 0,
            status_errors: 0,
            irq_level: false,
            eeprom,
            eng: [Engine::default(); ENGINES],
            rate: 48000,
            gpio_pins: 0,
            frame_bits: [0; 2],
            frame_word: [0; 2],
            ak4381: [0; AK4381_REGS],
            ak4355: [0; AK4355_REGS],
            ac97: [0; AC97_REGS],
            stats: Stats::default(),
            timer: None,
            timer_gen: 0,
            timer_due: 0,
            scratch: Vec::new(),
        };
        s.reset_registers(variant, false);
        s.reset_codecs();
        s
    }

    /// The registers as the chip powers up, or after a soft reset, which
    /// keeps CCS04 to CCS07 (see the module notes).
    fn reset_registers(&mut self, variant: Variant, soft: bool) {
        let mut keep = [0u8; 4];
        keep.copy_from_slice(&self.ccs[CCS_SYS_CONFIG as usize..CCS_SYS_CONFIG as usize + 4]);
        self.ccs = [0; CCS_SIZE as usize];
        self.mt = [0; MT_SIZE as usize];
        self.ccs[CCS_INT_MASK as usize] = 0xfe;
        self.ccs[CCS_SYS_CONFIG as usize] = 0x0f;
        self.ccs[CCS_I2S_FEATURES as usize] = 0x01;
        self.ccs[CCS_SPDIF_CONFIG as usize] = 0x01;
        self.ccs[CCS_GPIO_MASK_LO as usize] = 0xff;
        self.ccs[CCS_GPIO_MASK_LO as usize + 1] = 0xff;
        self.ccs[CCS_GPIO_MASK_HI as usize] = 0xff;
        if variant.eeprom_fitted() {
            self.ccs[CCS_I2C_STATUS as usize] = CCS_I2C_EEPROM;
        }
        self.mt[MT_MASK as usize] = 0xff;
        self.rate = 48000;
        self.eng = [Engine::default(); ENGINES];
        self.gpio_pins = self.pins();
        self.frame_bits = [0; 2];
        if soft {
            self.ccs[CCS_SYS_CONFIG as usize..CCS_SYS_CONFIG as usize + 4].copy_from_slice(&keep);
            self.ccs[CCS_CONTROL as usize] = CCS_CONTROL_SOFT_RESET;
        }
    }

    fn reset_codecs(&mut self) {
        self.ak4381 = [0; AK4381_REGS];
        self.ak4355 = [0; AK4355_REGS];
        // Both parts reset their attenuation registers to full level.
        self.ak4381[3] = 0xff;
        self.ak4381[4] = 0xff;
        self.ak4355[4..10].fill(0xff);
        self.ac97 = [0; AC97_REGS];
    }

    fn ccs(&self, reg: u32) -> u8 {
        self.ccs[reg as usize]
    }

    fn mt(&self, reg: u32) -> u8 {
        self.mt[reg as usize]
    }

    /// Dwords in one frame of an engine: the interleaved engine carries 8,
    /// 6, 4 or 2 slots by MT19, every other engine a stereo pair.
    fn frame_dwords(&self, idx: usize) -> u32 {
        if idx == ENG_PDMAI {
            8 - 2 * (self.mt(MT_CHANNELS) & 3) as u32
        } else {
            2
        }
    }

    fn engine_available(&self, idx: usize) -> bool {
        let streams = self.mt(MT_CHANNELS) & 3;
        match idx {
            ENG_PDMA1 => streams == 3,
            ENG_PDMA2 => streams >= 2,
            ENG_PDMA3 => streams >= 1,
            _ => true,
        }
    }

    fn engine_clocked(&self, idx: usize) -> bool {
        self.mt(MT_START) & (1 << idx) != 0
            && self.mt(MT_PAUSE) & (1 << idx) == 0
            && self.engine_available(idx)
            && self.ccs(CCS_POWER_DOWN) & CCS_PD_MT_CLOCK == 0
    }

    fn count_mask(idx: usize) -> u32 {
        if idx == ENG_PDMAI {
            0x7_ffff
        } else {
            0xffff
        }
    }

    /// A source raises its MT00 bit unless MT03 masks it. Approximation: a
    /// masked source does not latch (see the module notes).
    fn raise(&mut self, bit: usize) {
        if self.mt(MT_MASK) & (1 << bit) == 0 {
            self.mt[MT_STATUS as usize] |= 1 << bit;
        }
    }

    fn gpio_data(&self) -> u32 {
        u32::from(self.ccs(CCS_GPIO_DATA_LO))
            | u32::from(self.ccs(CCS_GPIO_DATA_LO + 1)) << 8
            | u32::from(self.ccs(CCS_GPIO_DATA_HI) & 0x7f) << 16
    }

    fn gpio_direction(&self) -> u32 {
        (u32::from(self.ccs(CCS_GPIO_DIR))
            | u32::from(self.ccs(CCS_GPIO_DIR + 1)) << 8
            | u32::from(self.ccs(CCS_GPIO_DIR + 2)) << 16)
            & GPIO_MASK_ALL
    }

    /// The level on each pin: an output drives its last written value, an
    /// input floats to its weak pull-up.
    fn pins(&self) -> u32 {
        let dir = self.gpio_direction();
        (self.gpio_data() & dir) | (GPIO_MASK_ALL & !dir)
    }

    fn ak_reg(&mut self, chip: u32, reg: usize) -> Option<&mut u8> {
        if chip == AK4381_CHIP_ADDRESS {
            self.ak4381.get_mut(reg)
        } else {
            self.ak4355.get_mut(reg)
        }
    }

    fn ac97_ready(&self, variant: Variant) -> bool {
        variant == Variant::Ac97 && self.ccs(CCS_AC_CONFIG) & CCS_AC_I2S == 0 && self.mt(MT_AC97_CMD) & MT_AC97_COLD_RESET == 0
    }
}

/// The DM8 audio function. The timer callback holds a weak reference to the
/// same state.
pub struct Dm8 {
    inner: Arc<Inner>,
}

impl Deref for Dm8 {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.inner
    }
}

impl Dm8 {
    pub fn new(variant: Variant) -> Self {
        Self { inner: Arc::new_cyclic(|me| Inner::new(variant, me.clone())) }
    }

    /// Name the card's state lock in the lock registry, which holds only a
    /// weak reference to it.
    pub fn register_locks(&self) {
        let me = Arc::downgrade(&self.inner);
        crate::locks::register_lock("dm8::state", move || me.upgrade().is_some_and(|i| i.state.is_locked()));
    }
}

/// Opens the host input, in place of the real one, in tests.
#[cfg(test)]
type InputOpener = Arc<dyn Fn(&AudioConfig) -> Option<crate::host_audio::HostIn> + Send + Sync>;

/// Lock order: `tick_pass`, then `state`, then `sink`; `stopped_at` (in
/// [`CardClock`]) is a leaf, taken by `now` with or without `state` held. Engines are moved with `state` held:
/// the DMA goes down through the bridge and memory, which never call back
/// into the card. The interrupt line is driven on a change, with `state`
/// held, as the other PCI functions do.
pub struct Inner {
    variant: Variant,
    pci: PciConfig,
    state: Mutex<State>,
    /// The sample clock and the timer bookkeeping shared with other audio
    /// cards; reached through `Deref`.
    clock: CardClock,
    timers: OnceLock<Arc<TimerManager>>,
    dma: OnceLock<Arc<dyn PciDma>>,
    irq: OnceLock<IrqLine>,
    /// `[audio]` settings once host output is enabled.
    audio_cfg: Mutex<Option<AudioConfig>>,
    sink: Mutex<Option<PcmOut>>,
    /// The host input the first record engine reads, once opened. Lock
    /// order: `state`, then `sink` or `source`, never both.
    source: Mutex<Option<PcmIn>>,
    /// Whether `[audio] capture` is on, and whether the first record engine
    /// is running: what the timer checks before it does anything about the
    /// input.
    capture_on: AtomicBool,
    capture_active: AtomicBool,
    capture_failure_logged: AtomicBool,
    /// For tests: opens the host input in place of the real one.
    #[cfg(test)]
    opener: Mutex<Option<InputOpener>>,
    /// Accesses that nothing establishes, counted.
    unexpected: AtomicU64,
    /// The messages behind them, for tests.
    #[cfg(test)]
    seen: Mutex<Vec<String>>,
    me: Weak<Inner>,
}

impl Deref for Inner {
    type Target = CardClock;
    fn deref(&self) -> &CardClock {
        &self.clock
    }
}

impl Inner {
    fn new(variant: Variant, me: Weak<Inner>) -> Self {
        let pci = PciConfig::new(
            PCI_VENDOR_ICE,
            PCI_DEVICE_ICE1724,
            PCI_CLASS_AUDIO,
            DM8_REVISION,
            [Bar::Io(CCS_SIZE), Bar::Io(MT_SIZE), Bar::Unused, Bar::Unused, Bar::Unused, Bar::Unused],
            PCI_INTA,
        );
        let card = Self {
            variant,
            pci,
            state: Mutex::new(State::new(variant)),
            clock: CardClock::new(),
            timers: OnceLock::new(),
            dma: OnceLock::new(),
            irq: OnceLock::new(),
            audio_cfg: Mutex::new(None),
            sink: Mutex::new(None),
            source: Mutex::new(None),
            capture_on: AtomicBool::new(false),
            capture_active: AtomicBool::new(false),
            capture_failure_logged: AtomicBool::new(false),
            #[cfg(test)]
            opener: Mutex::new(None),
            unexpected: AtomicU64::new(0),
            #[cfg(test)]
            seen: Mutex::new(Vec::new()),
            me,
        };
        card.config_defaults();
        card
    }

    /// Log an access or value that nothing establishes, and count it.
    fn note(&self, args: fmt::Arguments) {
        self.unexpected.fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        self.seen.lock().push(args.to_string());
        crate::dlog!(LogModule::Dm8, "dm8: {args}");
    }

    /// How many accesses or values nothing establishes the card has seen.
    pub fn unexpected(&self) -> u64 {
        self.unexpected.load(Ordering::Relaxed)
    }

    /// Play the output engines through the host with `audio`'s settings. The
    /// host stream opens when the machine starts, before the guest runs.
    pub fn enable_output(&self, audio: AudioConfig) {
        self.capture_on.store(audio.capture, Ordering::Relaxed);
        *self.audio_cfg.lock() = Some(audio);
    }

    /// Give the card the machine's timers. Called before `Device::start`.
    pub fn set_timer_manager(&self, tm: Arc<TimerManager>) {
        let _ = self.timers.set(tm);
    }

    // ---- clock and timer ----

    /// Whether the timer may be armed: started, on a real clock, with a
    /// manager.
    fn timers_live(&self) -> Option<&Arc<TimerManager>> {
        if !self.armed.load(Ordering::SeqCst) || self.fake_ns.load(Ordering::Relaxed) != i64::MIN {
            return None;
        }
        self.timers.get()
    }

    /// When the next running engine's interrupt interval ends, or a flow
    /// tick, whichever is first; `None` when no engine runs.
    fn next_deadline(&self, s: &State, now: i64) -> Option<i64> {
        let mut next: Option<i64> = None;
        for (i, e) in s.eng.iter().enumerate() {
            if !e.active {
                continue;
            }
            let mut when = now + FLOW_NS;
            if s.mt(MT_MASK) & (1 << i) == 0 {
                let frames = (e.tc - e.tc_pos + 1).div_ceil(s.frame_dwords(i));
                let ns = (u128::from(e.frames + u64::from(frames)) * NS_PER_SEC).div_ceil(u128::from(s.rate)) as i64;
                when = when.min((e.anchor + ns).max(now));
            }
            next = Some(next.map_or(when, |n| n.min(when)));
        }
        next
    }

    /// Replace the pending timer with one for the engines' present state.
    fn arm_timer(&self, s: &mut State, now: i64) {
        s.timer_gen += 1;
        if let Some(id) = s.timer.take() {
            if let Some(tm) = self.timers.get() {
                tm.remove(id);
            }
        }
        let Some(tm) = self.timers_live() else { return };
        let Some(when) = self.next_deadline(s, now) else { return };
        s.timer_due = when;
        let (me, gen) = (self.me.clone(), s.timer_gen);
        s.timer = Some(tm.add_one_shot(Duration::from_nanos((when - now).max(0) as u64), (), move |_| {
            let Some(i) = me.upgrade() else { return TimerReturn::Delete };
            i.timer_fire(gen)
        }));
    }

    /// The timer fired: bring the engines up to date and wait for the next
    /// deadline.
    fn timer_fire(&self, gen: u64) -> TimerReturn {
        let _pass = self.tick_pass.lock();
        if self.stopped() {
            return TimerReturn::Delete;
        }
        self.reopen_sink_if_due();
        self.open_source_if_wanted();
        let mut s = self.state.lock();
        if s.timer_gen != gen {
            return TimerReturn::Delete;
        }
        // A timer far behind its deadline means the host held up this
        // thread; the clock is set back rather than replaying the time in a
        // burst (see the module notes).
        let now = self.absorb_stall(s.timer_due, FLOW_NS);
        self.sync(&mut s, now);
        match self.next_deadline(&s, now) {
            Some(when) => {
                s.timer_due = when;
                TimerReturn::RescheduleOneShot(Duration::from_nanos((when - now).max(0) as u64))
            }
            None => {
                s.timer = None;
                TimerReturn::Delete
            }
        }
    }

    fn pause(&self) {
        self.stop_clock();
        self.armed.store(false, Ordering::SeqCst);
        {
            let mut s = self.state.lock();
            s.timer_gen += 1;
            if let Some(id) = s.timer.take() {
                if let Some(tm) = self.timers.get() {
                    tm.remove(id);
                }
            }
        }
        // Once this returns no timer callback is under way.
        drop(self.tick_pass.lock());
    }

    fn resume(&self) {
        self.open_sink(true);
        self.start_clock();
        self.armed.store(true, Ordering::SeqCst);
        let now = self.now();
        let mut s = self.state.lock();
        self.arm_timer(&mut s, now);
    }

    // ---- host audio ----

    /// Open the host stream when output is enabled and none is open. `force`
    /// skips the wait between tries.
    fn open_sink(&self, force: bool) {
        let Some(cfg) = self.audio_cfg.lock().clone() else { return };
        // Read before the sink lock: engines take the state lock, then the
        // sink's.
        let rate = self.state.lock().rate;
        let mut sink = self.sink.lock();
        let now = Instant::now();
        if let Some(sk) = sink.as_ref() {
            if !(sk.host_retry_due(now) || force && sk.host.is_none()) {
                return;
            }
        }
        let ring = |rate: u32| rate as usize * 2 / 5;
        let host = crate::host_audio::open_output_channels(&cfg, MAX_HOST_CHANNELS, Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(true)), ring, "DM8 audio", "IRIS_DM8_AUDIO_CAPTURE");
        crate::dlog!(LogModule::Dm8, "dm8: host audio {}", if host.is_some() { "open" } else { "failed to open" });
        let host = host.map(|out| {
            let rs = FrameResampler::new(rate, out.stream_rate, out.channels);
            (out, rs)
        });
        let failed_at = host.is_none().then_some(now);
        match sink.as_mut() {
            Some(sk) => {
                sk.host = host;
                sk.host_failed_at = failed_at;
            }
            None => *sink = Some(PcmOut::new(host, failed_at)),
        }
    }

    /// Retry a failed host stream open from the timer, before it takes the
    /// state lock.
    fn reopen_sink_if_due(&self) {
        let due = self.sink.lock().as_ref().is_some_and(|sk| sk.host_retry_due(Instant::now()));
        if due {
            self.open_sink(false);
        }
    }

    /// Open the host input when the first record engine runs, capture is on
    /// and none is open (or the open one has died). Called from the timer
    /// before it takes the state lock. Nothing is held across the open,
    /// which can take hundreds of milliseconds (a server connection, a
    /// permission prompt): register accesses reach `source` only for an
    /// instant, and the timer's own hold-up the clock absorbs. A failure is
    /// reported once and retried at the output's pace; the engine records
    /// silence meanwhile. With capture off or the engine idle this is two
    /// atomic loads.
    fn open_source_if_wanted(&self) {
        if !self.capture_on.load(Ordering::Relaxed) || !self.capture_active.load(Ordering::Relaxed) {
            return;
        }
        let now = Instant::now();
        if self.source.lock().as_ref().is_some_and(|sk| !sk.host_retry_due(now)) {
            return;
        }
        let Some(cfg) = self.audio_cfg.lock().clone() else { return };
        let host = self.open_input(&cfg);
        if host.is_none() && !self.capture_failure_logged.swap(true, Ordering::Relaxed) {
            eprintln!("DM8 audio: no host input; recording silence");
            crate::dlog!(LogModule::Dm8, "dm8: host input failed to open");
        }
        let failed_at = host.is_none().then_some(now);
        // The old stream and, if the engine stopped during the open, the
        // new one are dropped with no lock held.
        let _old = self.source.lock().replace(PcmIn::new(host, failed_at));
        if !self.capture_active.load(Ordering::Relaxed) {
            let _late = self.source.lock().take();
        }
    }

    fn open_input(&self, cfg: &AudioConfig) -> Option<crate::host_audio::HostIn> {
        #[cfg(test)]
        if let Some(f) = self.opener.lock().clone() {
            return f(cfg);
        }
        let ring = |rate: u32| rate as usize * 2 / 5;
        crate::host_audio::open_input(cfg, ring, "DM8 audio")
    }

    // ---- interrupts ----

    /// Recompute the controller's interrupt bits and drive the line if its
    /// level changed. Inferred: the macro bit (CCS02 bit 4) is the OR of the
    /// unmasked MT00 bits, and INTA# is asserted while any unmasked CCS02
    /// bit is set.
    fn update_irq(&self, s: &mut State) {
        if s.mt(MT_STATUS) & !s.mt(MT_MASK) != 0 {
            s.ccs[CCS_INT_STATUS as usize] |= CCS_INT_MT;
        } else {
            s.ccs[CCS_INT_STATUS as usize] &= !CCS_INT_MT;
        }
        let level = s.ccs(CCS_INT_STATUS) & !s.ccs(CCS_INT_MASK) & (CCS_INT_MPU_RX | CCS_INT_MPU_TX | CCS_INT_MT) != 0;
        if level != s.irq_level {
            s.irq_level = level;
            if let Some(l) = self.irq.get() {
                l.set(level);
            }
        }
    }

    // ---- DMA engines ----

    /// Move `dwords` through one engine, no more than one chunk, and no
    /// further than the end of its buffer per DMA transfer.
    fn move_dwords(&self, s: &mut State, idx: usize, dwords: u32) {
        debug_assert!(dwords <= CHUNK_DWORDS);
        let record = idx == ENG_RDMA0 || idx == ENG_RDMA1;
        let frame = s.frame_dwords(idx);
        let bytes = dwords as usize * 4;
        let mut buf = std::mem::take(&mut s.scratch);
        // Recording: the first engine reads the host input, or silence; the
        // second records silence.
        buf.clear();
        buf.resize(bytes, 0);
        if idx == ENG_RDMA0 {
            if let Some(src) = self.source.lock().as_mut() {
                src.fill(&mut buf, s.rate);
            }
        }
        let dma = self.dma.get().filter(|_| self.pci.command() & PCI_COMMAND_BUS_MASTER != 0);
        let mut done = 0u32;
        let mut failed = false;
        while done < dwords {
            let e = &mut s.eng[idx];
            let span = (dwords - done).min(e.count + 1 - e.pos);
            let at = u64::from(e.addr & !3) + 4 * u64::from(e.pos);
            let part = &mut buf[done as usize * 4..(done + span) as usize * 4];
            let ok = match dma {
                Some(d) if record => d.dma_write(at, part).is_ok(),
                Some(d) => d.dma_read(at, part).is_ok(),
                None => false,
            };
            if !ok {
                failed = true;
                if !record {
                    part.fill(0);
                }
            }
            done += span;
            e.pos += span;
            if e.pos > e.count {
                e.pos = 0;
            }
            let interval = e.tc + 1;
            let moved = e.tc_pos + span;
            e.tc_pos = moved % interval;
            if moved / interval != 0 {
                s.raise(idx);
            }
        }

        if failed {
            // Approximation: see the module notes on a failed transfer.
            self.note(format_args!("DMA engine {idx} transfer failed"));
            s.status_errors |= STATUS_MASTER_ABORT as u16;
            s.mt[MT_FIFO_ERROR as usize] |= 1 << idx;
            s.raise(FIFO_ERROR_SOURCE);
        }

        let frames = u64::from(dwords / frame);
        match idx {
            ENG_PDMAI => {
                if let Some(sink) = self.sink.lock().as_mut() {
                    // Each slot's top 16 bits; the host side folds the slots
                    // to its speakers.
                    sink.analog_stage(frame as usize).extend(buf.as_chunks::<4>().0.iter().map(|d| i16::from_le_bytes([d[2], d[3]])));
                }
                if !failed {
                    s.stats.analog_last = u32::from_le_bytes(buf[bytes - 4..].try_into().unwrap());
                }
                s.stats.analog_frames += frames;
            }
            ENG_PDMA4 => {
                if let Some(sink) = self.sink.lock().as_mut() {
                    sink.spdif_stage().extend(buf.as_chunks::<4>().0.iter().map(|d| i16::from_le_bytes([d[2], d[3]])));
                }
                s.stats.spdif_frames += frames;
            }
            ENG_RDMA0 => s.stats.capture_frames += frames,
            _ => {}
        }
        s.scratch = buf;
    }

    /// Move the frames an engine's clock has made due since its anchor.
    fn advance(&self, s: &mut State, idx: usize, now: i64) {
        let frame = s.frame_dwords(idx);
        let e = &s.eng[idx];
        let due = (u128::from((now - e.anchor).max(0) as u64) * u128::from(s.rate) / NS_PER_SEC) as u64;
        if due > e.frames + u64::from(s.rate) * MAX_CATCHUP_SECONDS {
            // Approximation: a clock jump of more than a second is not
            // replayed through memory; the engine resumes at the present.
            self.note(format_args!("DMA clock jump skipped"));
            s.eng[idx].frames = due;
            return;
        }
        while s.eng[idx].frames < due {
            let frames = ((due - s.eng[idx].frames) as u32).min(CHUNK_DWORDS / frame);
            self.move_dwords(s, idx, frames * frame);
            s.eng[idx].frames += u64::from(frames);
        }
    }

    /// Bring every running engine up to the present.
    fn sync(&self, s: &mut State, now: i64) {
        for i in 0..ENGINES {
            if s.eng[i].active {
                self.advance(s, i, now);
            }
        }
        if let Some(sink) = self.sink.lock().as_mut() {
            if let Some(dropped) = sink.flush(s.rate, s.eng[ENG_PDMAI].active, s.eng[ENG_PDMA4].active) {
                s.stats.dropped_frames += dropped;
            }
        }
        self.update_irq(s);
    }

    /// Follow a change of start, pause, availability, rate or clock gating:
    /// engines that begin running anchor their clock at the present, and
    /// every running engine's anchor restarts when `restart` is set.
    /// Callers sync before they change the state.
    fn clock_changed(&self, s: &mut State, now: i64, restart: bool) {
        let mut capture_started = false;
        for i in 0..ENGINES {
            let clocked = s.engine_clocked(i);
            let e = &mut s.eng[i];
            if clocked && (!e.active || restart) {
                e.anchor = now;
                e.frames = 0;
                capture_started |= i == ENG_RDMA0;
            }
            e.active = clocked;
        }
        let recording = s.eng[ENG_RDMA0].active;
        let was_recording = self.capture_active.swap(recording, Ordering::Relaxed);
        if capture_started {
            // Whatever the host input gathered while nothing read it is old.
            if let Some(src) = self.source.lock().as_mut() {
                src.restart();
            }
        }
        if was_recording && !recording {
            // Close the input. Dropping a stream can block, and this runs
            // under the state lock, so another thread does it.
            if let Some(old) = self.source.lock().take() {
                std::thread::spawn(move || drop(old));
            }
        }
        self.arm_timer(s, now);
    }

    fn apply_rate(&self, s: &mut State) {
        let rate_reg = s.mt(MT_RATE);
        let code = rate_reg & 0xf;
        let rate = RATES[code as usize];
        if rate_reg & 0x10 != 0 {
            // Approximation: the external clock has no source here.
            self.note(format_args!("external master clock is not modelled"));
        }
        if rate == 0 {
            self.note(format_args!("reserved sample rate code {code}"));
            return;
        }
        if code == 14 && s.ccs(CCS_SYS_CONFIG) & CCS_SYS_CRYSTAL_49 == 0 && s.mt(MT_FORMAT) & MT_FORMAT_128X == 0 {
            self.note(format_args!("192 kHz needs a 49.152 MHz crystal or 128x"));
        }
        s.rate = rate;
    }

    // ---- converters behind the GPIO pins ----

    fn codec_latch(&self, s: &mut State, port: usize) {
        let chip = if port == 1 { AK4355_CHIP_ADDRESS } else { AK4381_CHIP_ADDRESS };
        let word = s.frame_word[port];
        let header = u32::from(word >> 8);
        let reg = (header & 0x1f) as usize;
        if s.frame_bits[port] == 0 {
            // A select pulse with no clock is not a frame.
            return;
        }
        if s.frame_bits[port] != 16 {
            self.note(format_args!("converter frame of {} bits ignored", s.frame_bits[port]));
            s.stats.codec_rejects += 1;
            return;
        }
        // The chip address is fixed in each part and the frame is write-only.
        if header >> 6 != chip || header & 0x20 == 0 {
            self.note(format_args!("converter frame {word:#06x} is not a write to chip {chip}"));
            s.stats.codec_rejects += 1;
            return;
        }
        let Some(slot) = s.ak_reg(chip, reg) else {
            // The parts say these addresses must not be written.
            self.note(format_args!("write to undefined register {reg} of chip {chip}"));
            s.stats.codec_rejects += 1;
            return;
        };
        *slot = word as u8;
        if chip == AK4381_CHIP_ADDRESS {
            s.stats.ak4381_writes += 1;
        } else {
            s.stats.ak4355_writes += 1;
        }
    }

    /// Follow the converter pins after a GPIO register change. Each part
    /// sees the shared CCLK and CDOUT lines and its own select: a frame
    /// starts when the select falls, takes a bit on each rising CCLK edge
    /// while the select is held across the change, and takes effect when
    /// the select rises. Edges that arrive in one register write are taken
    /// as the parts' timing orders them: a select that rises or falls with
    /// CCLK does not see that clock edge. Approximation: every complete
    /// frame is accepted (see the module notes).
    fn codec_pins(&self, s: &mut State) {
        const SELECTS: [u32; 2] = [GPIO_CS_FRONT, GPIO_CS_SURROUND];
        let pins = s.pins();
        let old = s.gpio_pins;
        let clock_rise = pins & GPIO_CCLK != 0 && old & GPIO_CCLK == 0;
        s.gpio_pins = pins;
        if self.variant == Variant::Ac97 {
            return;
        }
        for (port, sel) in SELECTS.into_iter().enumerate() {
            let was = old & sel == 0;
            let now = pins & sel == 0;
            if clock_rise && was && now {
                if s.frame_bits[port] < 16 {
                    s.frame_word[port] = s.frame_word[port] << 1 | u16::from(pins & GPIO_CDOUT != 0);
                }
                s.frame_bits[port] = s.frame_bits[port].saturating_add(1);
            }
            if now && !was {
                s.frame_bits[port] = 0;
                s.frame_word[port] = 0;
            } else if was && !now {
                self.codec_latch(s, port);
                s.frame_bits[port] = 0;
            }
        }
    }

    // ---- I2C port and EEPROM ----

    fn i2c_start(&self, s: &mut State) {
        let cmd = s.ccs(CCS_I2C_DEVICE);
        let device = u32::from(cmd >> 1);
        let write = cmd & 1 != 0;
        // Approximation: a transfer finishes at once; one nothing answers
        // is logged and leaves the data register alone.
        if device != EEPROM_I2C_ADDRESS || !self.variant.eeprom_fitted() {
            self.note(format_args!("I2C {} of device {device:#04x} has no answer modelled", if write { "write" } else { "read" }));
            return;
        }
        let at = s.ccs(CCS_I2C_BYTE) as usize;
        if write {
            s.eeprom[at] = s.ccs(CCS_I2C_DATA);
        } else {
            s.ccs[CCS_I2C_DATA as usize] = s.eeprom[at];
        }
    }

    // ---- AC'97 port ----

    fn ac97_cycle(&self, s: &mut State, cmd: u8) {
        let index = ((s.mt(MT_AC97_INDEX) & 0x7e) >> 1) as usize;
        if s.ccs(CCS_AC_CONFIG) & CCS_AC_I2S != 0 {
            self.note(format_args!("AC'97 cycle in I2S mode has no meaning"));
            return;
        }
        if !s.ac97_ready(self.variant) || cmd & 3 != 0 {
            // Approximation: only one codec, ready once the cold reset is
            // released, exists. A cycle with none to answer does nothing
            // and reads back zero.
            self.note(format_args!("AC'97 cycle with no codec to answer (command {cmd:#04x})"));
            if cmd & MT_AC97_READ != 0 {
                s.mt[MT_AC97_DATA as usize] = 0;
                s.mt[MT_AC97_DATA as usize + 1] = 0;
            }
            return;
        }
        if cmd & MT_AC97_WRITE != 0 {
            s.ac97[index] = u16::from(s.mt(MT_AC97_DATA)) | u16::from(s.mt(MT_AC97_DATA + 1)) << 8;
        } else {
            let v = s.ac97[index];
            s.mt[MT_AC97_DATA as usize] = v as u8;
            s.mt[MT_AC97_DATA as usize + 1] = (v >> 8) as u8;
        }
    }

    // ---- reset ----

    /// RST#: the chip as it powers up, configuration space included. The
    /// machine calls this after the bridge has reset the function's header
    /// (the header's interrupt line is set here, after that reset, as the
    /// chip powers up with 0xff there). The EEPROM keeps what was written to
    /// it, and the converter counters keep counting.
    pub fn reset(&self) {
        let now = self.now();
        {
            let mut s = self.state.lock();
            self.reset_state(&mut s, now);
        }
        self.config_defaults();
    }

    fn reset_state(&self, s: &mut State, now: i64) {
        s.reset_registers(self.variant, false);
        s.reset_codecs();
        s.pm_state = 0;
        s.status_errors = 0;
        self.clock_changed(s, now, true);
        self.update_irq(s);
    }

    /// The header values the chip powers up with that the bridge's reset
    /// does not restore.
    fn config_defaults(&self) {
        self.pci.write(3, 0);
        self.pci.write(15, 0xff);
        let id = if self.variant.eeprom_fitted() { SUBSYSTEM_DM8 } else { SUBSYSTEM_NO_EEPROM };
        self.pci.write(SUBSYSTEM_DWORD, u32::from(id) << 16 | u32::from(PCI_VENDOR_ICE));
        self.pci.write(SVID_MASK_DWORD, 0);
        self.pci.write(PMCSR_DWORD, 0);
    }

    fn soft_reset(&self, s: &mut State, now: i64) {
        s.reset_registers(self.variant, true);
        self.clock_changed(s, now, true);
        self.update_irq(s);
    }

    // ---- controller block registers (BAR0) ----

    fn ccs_read(&self, s: &State, off: u32) -> u8 {
        let value = s.ccs(off);
        match off {
            CCS_UART_TX_COUNT | CCS_UART_RX_COUNT => 0,
            CCS_UART_DATA | CCS_UART_COMMAND => {
                self.note(format_args!("MPU-401 UART read at CCS {off:#04x}"));
                value
            }
            CCS_GPIO_DATA_LO | 0x15 => (s.pins() >> ((off - CCS_GPIO_DATA_LO) * 8)) as u8,
            CCS_GPIO_DATA_HI => (s.pins() >> 16) as u8,
            0x1a => value & 0x7f,
            0x03 | 0x08 | 0x09 | 0x0f | 0x1b | 0x1d => {
                self.note(format_args!("read of unassigned CCS {off:#04x}"));
                0
            }
            _ => value,
        }
    }

    fn ccs_write(&self, s: &mut State, off: u32, v: u8, now: i64) {
        let o = off as usize;
        match off {
            CCS_CONTROL => {
                s.ccs[o] = v;
                if v & CCS_CONTROL_SOFT_RESET != 0 {
                    self.soft_reset(s, now);
                }
            }
            CCS_INT_MASK => s.ccs[o] = v,
            // Write one to clear; the macro bit follows MT00.
            CCS_INT_STATUS => s.ccs[o] &= !(v & CCS_INT_STATUS_WRITABLE),
            CCS_SYS_CONFIG | CCS_I2S_FEATURES | CCS_I2C_BYTE | CCS_I2C_DATA => s.ccs[o] = v,
            CCS_GPIO_DIR | 0x19 => {
                s.ccs[o] = v;
                self.codec_pins(s);
            }
            0x1a => {
                s.ccs[o] = v & 0x7f;
                self.codec_pins(s);
            }
            CCS_AC_CONFIG => s.ccs[o] = v & 0x82,
            // Bit 6, the transmitter-implemented flag, is read only.
            CCS_SPDIF_CONFIG => s.ccs[o] = (s.ccs[o] & 0x40) | (v & 0xbf),
            CCS_UART_DATA | CCS_UART_COMMAND | CCS_UART_SETTING => {
                s.ccs[o] = v;
                self.note(format_args!("MPU-401 UART write at CCS {off:#04x}"));
            }
            CCS_UART_TX_COUNT | CCS_UART_RX_COUNT => self.note(format_args!("write to read-only CCS {off:#04x}")),
            CCS_I2C_DEVICE => {
                s.ccs[o] = v;
                self.i2c_start(s);
            }
            // Bit 7 is the EEPROM strap and bit 0 is busy: both read only.
            CCS_I2C_STATUS => s.ccs[o] = (s.ccs[o] & 0x81) | (v & 0x02),
            CCS_GPIO_DATA_LO | 0x15 => {
                // Only the bits whose write-mask bit is clear take the value.
                let mask = s.ccs[(CCS_GPIO_MASK_LO + off - CCS_GPIO_DATA_LO) as usize];
                s.ccs[o] = (s.ccs[o] & mask) | (v & !mask);
                self.codec_pins(s);
            }
            CCS_GPIO_DATA_HI => {
                let mask = s.ccs(CCS_GPIO_MASK_HI);
                s.ccs[o] = (s.ccs[o] & mask) | (v & !mask & 0x7f);
                self.codec_pins(s);
            }
            CCS_GPIO_MASK_LO | 0x17 | CCS_GPIO_MASK_HI => s.ccs[o] = v,
            CCS_POWER_DOWN => {
                s.ccs[o] = v & 0xbd;
                self.clock_changed(s, now, false);
            }
            _ => self.note(format_args!("write of {v:#04x} to unassigned CCS {off:#04x}")),
        }
    }

    // ---- multi-channel block registers (BAR1) ----

    fn mt_read(&self, s: &State, off: u32) -> u8 {
        if let Some((idx, field, byte)) = engine_decode(off) {
            let e = &s.eng[idx];
            let word = match field {
                Field::Addr => (e.addr & !3).wrapping_add(4 * e.pos),
                Field::Count => e.count - e.pos,
                Field::Tc => {
                    // Approximation: terminal counts are write only.
                    self.note(format_args!("read of write-only terminal count {off:#04x}"));
                    0
                }
            };
            return (word >> (8 * byte)) as u8;
        }
        let value = s.mt(off);
        match off {
            MT_STATUS | MT_RATE | MT_FORMAT | MT_MASK | MT_START | MT_CHANNELS | MT_FIFO_ERROR | MT_PAUSE | MT_PEAK_INDEX
            | MT_ROUTING..=0x2f | MT_SPDIF_CTRL | 0x3d => value,
            MT_AC97_INDEX | MT_AC97_DATA | 0x07 => {
                if self.variant != Variant::Ac97 || s.ccs(CCS_AC_CONFIG) & CCS_AC_I2S != 0 {
                    self.note(format_args!("read of AC'97 register {off:#04x} with no AC-link codec"));
                }
                value
            }
            MT_AC97_CMD => (value & !(MT_AC97_READY | 0x34)) | if s.ac97_ready(self.variant) { MT_AC97_READY } else { 0 },
            MT_PEAK_DATA => {
                // Approximation: the peak meters are not modelled.
                self.note(format_args!("peak meters are not modelled"));
                0
            }
            _ => {
                self.note(format_args!("read of unassigned MT {off:#04x}"));
                0
            }
        }
    }

    fn engine_write(&self, s: &mut State, idx: usize, field: Field, byte: u32, v: u8) {
        let mask = if field == Field::Addr { u32::MAX } else { State::count_mask(idx) };
        let e = &mut s.eng[idx];
        // Approximation: the effect of a write to a running engine is not
        // stated, so a write reloads the position or interval.
        let reg = e.field(field);
        *reg = (*reg & !(0xff << (8 * byte)) | u32::from(v) << (8 * byte)) & mask;
        if field == Field::Addr {
            *reg &= !3;
        }
        if field == Field::Tc {
            e.tc_pos = 0;
        } else {
            e.pos = 0;
        }
    }

    fn mt_write(&self, s: &mut State, off: u32, v: u8, now: i64) {
        if let Some((idx, field, byte)) = engine_decode(off) {
            self.engine_write(s, idx, field, byte, v);
            self.arm_timer(s, now);
            return;
        }
        let o = off as usize;
        match off {
            MT_STATUS => s.mt[o] &= !v,
            MT_RATE => {
                s.mt[o] = v & 0x1f;
                self.apply_rate(s);
                self.clock_changed(s, now, true);
            }
            MT_FORMAT => {
                s.mt[o] = v & 0x0f;
                if v & 3 != 0 {
                    self.note(format_args!("reserved data format {}", v & 3));
                }
                if v & 4 != 0 {
                    self.note(format_args!("MT02 bit 2 has no known meaning"));
                }
            }
            MT_MASK => {
                s.mt[o] = v;
                self.arm_timer(s, now);
            }
            MT_AC97_INDEX => s.mt[o] = v & 0x7f,
            MT_AC97_DATA | 0x07 => s.mt[o] = v,
            MT_AC97_CMD => {
                s.mt[o] = v & 0xc3;
                // Bit 7 drives PRST# to the converters in I2S mode too; no
                // converter reacts to it in this model.
                if v & (MT_AC97_WRITE | MT_AC97_READ) != 0 {
                    // Approximation: a cycle completes at once.
                    self.ac97_cycle(s, v);
                }
            }
            MT_START => {
                if v & !ENGINE_BITS != 0 {
                    self.note(format_args!("MT18 bit 3 is reserved"));
                }
                s.mt[o] = v & ENGINE_BITS;
                for i in 0..ENGINES {
                    if v & (1 << i) != 0 && i != FIFO_ERROR_SOURCE && !s.engine_available(i) {
                        self.note(format_args!("engine {i} started while MT19 gives its slots to the interleaved stream"));
                    }
                }
                self.clock_changed(s, now, false);
            }
            MT_CHANNELS => {
                s.mt[o] = v & 3;
                self.clock_changed(s, now, true);
            }
            MT_FIFO_ERROR => s.mt[o] &= !(v & ENGINE_BITS),
            MT_PAUSE => {
                s.mt[o] = v & ENGINE_BITS;
                self.clock_changed(s, now, false);
            }
            MT_ROUTING..=0x2f => {
                s.mt[o] = v;
                if v != 0 {
                    self.note(format_args!("output routing is not modelled"));
                }
            }
            MT_SPDIF_CTRL | 0x3d => {
                // Every field but the validity flag (bit 15) must be written
                // only while the transmitter is disabled. Approximation: the
                // model enforces that rule (see the module notes).
                let mut v = v;
                if s.ccs(CCS_SPDIF_CONFIG) & 0x80 != 0 {
                    if off == MT_SPDIF_CTRL {
                        self.note(format_args!("MT3C fields written while the S/PDIF transmitter is enabled"));
                        return;
                    }
                    if (v ^ s.mt[o]) & 0x7f != 0 {
                        self.note(format_args!("MT3C fields written while the S/PDIF transmitter is enabled"));
                    }
                    v = (s.mt[o] & 0x7f) | (v & 0x80);
                }
                s.mt[o] = v;
            }
            MT_PEAK_INDEX => s.mt[o] = v & 0x1f,
            MT_PEAK_DATA => self.note(format_args!("write to read-only peak data")),
            _ => self.note(format_args!("write of {v:#04x} to unassigned MT {off:#04x}")),
        }
    }

    // ---- bus accesses ----

    fn read(&self, bar: usize, off: u32, width: usize) -> u32 {
        let size = if bar == 0 { CCS_SIZE } else { MT_SIZE };
        let now = self.now();
        let mut s = self.state.lock();
        if s.pm_state != 0 {
            drop(s);
            self.note(format_args!("read of BAR {bar} outside D0"));
            return u32::MAX >> (32 - 8 * width);
        }
        self.sync(&mut s, now);
        let mut v = 0u32;
        for i in 0..width as u32 {
            let o = off + i;
            let b = if o >= size {
                self.note(format_args!("read past the end of BAR {bar} at {o:#04x}"));
                0
            } else if bar == 0 {
                self.ccs_read(&s, o)
            } else {
                self.mt_read(&s, o)
            };
            v |= u32::from(b) << (8 * i);
        }
        v
    }

    fn write(&self, bar: usize, off: u32, val: u32, width: usize) {
        let size = if bar == 0 { CCS_SIZE } else { MT_SIZE };
        let now = self.now();
        let mut s = self.state.lock();
        if s.pm_state != 0 {
            drop(s);
            self.note(format_args!("write of {val:#x} to BAR {bar} outside D0"));
            return;
        }
        self.sync(&mut s, now);
        for i in 0..width as u32 {
            let o = off + i;
            let b = (val >> (8 * i)) as u8;
            if o >= size {
                self.note(format_args!("write past the end of BAR {bar} at {o:#04x}"));
            } else if bar == 0 {
                self.ccs_write(&mut s, o, b, now);
            } else {
                self.mt_write(&mut s, o, b, now);
            }
        }
        self.update_irq(&mut s);
    }

    // ---- configuration space ----

    /// Registers the chip leaves undescribed: BARs 2 and 3, the subsystem
    /// mask at its other offset, 0x43 and the hardware configuration
    /// control. Reads 0; writes are ignored.
    fn config_unknown(index: usize) -> bool {
        matches!(index, 6 | 7 | 24)
    }

    fn config_read(&self, index: usize) -> u32 {
        if Self::config_unknown(index) {
            self.note(format_args!("PCI config read of unknown register at {:#04x}", index * 4));
            return 0;
        }
        match index {
            1 => {
                let s = self.state.lock();
                let status = STATUS_BASE | u32::from(s.status_errors) | if s.irq_level { STATUS_INTX } else { 0 };
                self.pci.command() | status << 16
            }
            // The capability pointer.
            13 => PM_CAP_DWORD as u32 * 4,
            PM_CAP_DWORD => PM_CAP_HEADER,
            PMCSR_DWORD => u32::from(self.state.lock().pm_state),
            // Only bit 7 of the subsystem mask byte exists.
            SVID_MASK_DWORD => self.pci.read(index) & SVID_WRITE_ENABLE,
            _ if (16..64).contains(&index) => 0,
            _ => self.pci.read(index),
        }
    }

    fn config_write(&self, index: usize, val: u32) {
        if Self::config_unknown(index) || index == SVID_MASK_DWORD && val & !SVID_WRITE_ENABLE != 0 {
            self.note(format_args!("PCI config write of {val:#x} to unknown register at {:#04x}", index * 4));
        }
        match index {
            1 => {
                // I/O decode and bus mastering only; the status errors
                // clear on a write of one.
                self.pci.write(1, val & (crate::pci::PCI_COMMAND_IO | PCI_COMMAND_BUS_MASTER));
                self.state.lock().status_errors &= !((val >> 16 & STATUS_W1C) as u16);
            }
            // The cache line size reads 0; the latency timer keeps bits 7:3.
            3 => self.pci.write(3, val & 0xf800),
            4 | 5 => self.pci.write(index, val),
            // The line is the only writable byte; the pin is wiring.
            15 => self.pci.write(15, val & 0xff),
            // The subsystem pair is writable only while PCI42 bit 7 is set.
            SUBSYSTEM_DWORD => {
                if self.pci.read(SVID_MASK_DWORD) & SVID_WRITE_ENABLE != 0 {
                    self.pci.write(index, val);
                }
            }
            SVID_MASK_DWORD => self.pci.write(index, val & SVID_WRITE_ENABLE),
            PMCSR_DWORD => self.power_state_write(val),
            _ => {}
        }
    }

    /// PMCSR bits 1:0. D1 is not advertised and the write is refused; D3hot
    /// to D0 resets the chip including configuration space.
    fn power_state_write(&self, val: u32) {
        let new = (val & 3) as u8;
        if new == 1 {
            return;
        }
        let now = self.now();
        let old = {
            let mut s = self.state.lock();
            std::mem::replace(&mut s.pm_state, new)
        };
        // The bridge merges a later byte write against the header, so the
        // state is kept there too.
        self.pci.write(PMCSR_DWORD, u32::from(new));
        if old == 3 && new == 0 {
            self.pci.reset();
            let mut s = self.state.lock();
            self.reset_state(&mut s, now);
            drop(s);
            self.config_defaults();
        } else if old == 0 && new != 0 {
            self.note(format_args!("D2 and D3hot power-down of the blocks in CCS1C is not modelled beyond the clock gate"));
        }
    }

    // ---- observation ----

    /// The engines brought up to date, then what they and the converters
    /// have done.
    pub fn stats(&self) -> Stats {
        let now = self.now();
        let mut s = self.state.lock();
        self.sync(&mut s, now);
        s.stats
    }

    /// What the front DAC (AK4381) was last told, register by register.
    pub fn ak4381(&self) -> [u8; AK4381_REGS] {
        self.state.lock().ak4381
    }

    /// What the surround DAC (AK4355) was last told.
    pub fn ak4355(&self) -> [u8; AK4355_REGS] {
        self.state.lock().ak4355
    }
}

impl Device for Dm8 {
    fn step(&self, _cycles: u64) {}
    fn start(&self) {
        self.inner.resume()
    }
    fn stop(&self) {
        self.inner.pause()
    }
    fn is_running(&self) -> bool {
        self.inner.armed.load(Ordering::SeqCst)
    }
    fn get_clock(&self) -> u64 {
        0
    }
}

impl PciBusDevice for Dm8 {
    fn config(&self) -> &PciConfig {
        &self.inner.pci
    }

    fn config_dword(&self, index: usize) -> u32 {
        self.inner.config_read(index)
    }

    fn config_write(&self, index: usize, val: u32) {
        self.inner.config_write(index, val)
    }

    /// `off` is the byte offset the register file sees; a wider access is
    /// byte steps in address order.
    fn bar_read(&self, bar: usize, off: u32, width: usize) -> u32 {
        if bar > 1 {
            self.inner.note(format_args!("read of unimplemented BAR {bar}"));
            return 0;
        }
        self.inner.read(bar, off, width)
    }

    fn bar_write(&self, bar: usize, off: u32, val: u32, width: usize) {
        if bar > 1 {
            self.inner.note(format_args!("write to unimplemented BAR {bar}"));
            return;
        }
        self.inner.write(bar, off, val, width)
    }

    fn connect_interrupt(&self, line: IrqLine) {
        let i = &self.inner;
        let _ = i.irq.set(line);
        // Drive the line to what the chip is holding now.
        let mut s = i.state.lock();
        if let Some(l) = i.irq.get() {
            l.set(s.irq_level);
        }
        i.update_irq(&mut s);
    }

    fn connect_dma(&self, dma: Arc<dyn PciDma>) {
        let _ = self.inner.dma.set(dma);
    }
}

impl State {
    /// A loaded state must not make the card divide by zero or index past
    /// a buffer; refuse it, naming the field.
    fn check(&self) -> Result<(), String> {
        if !RATES.contains(&self.rate) || self.rate == 0 {
            return Err(format!("rate: {} Hz is not one the card runs at", self.rate));
        }
        for (i, e) in self.eng.iter().enumerate() {
            if e.count > Self::count_mask(i) || e.tc > Self::count_mask(i) {
                return Err(format!("engine {i}: count or interval past the register width"));
            }
            if e.pos > e.count || e.tc_pos > e.tc {
                return Err(format!("engine {i}: position past the end of its buffer or interval"));
            }
        }
        if self.pm_state > 3 || self.pm_state == 1 {
            return Err(format!("pm_state: {} is not a state the card enters", self.pm_state));
        }
        Ok(())
    }
}

impl Saveable for Dm8 {
    /// The registers and engines, with each running engine's age on the
    /// card's clock. The header is the bridge's to save; the host stream is
    /// this process's own and is not saved. The timer is armed again when the
    /// card starts.
    fn save_state(&self) -> toml::Value {
        let i = &self.inner;
        let now = i.now();
        let s = i.state.lock();
        let engines = s.eng.iter().map(|e| {
            StateTable::new()
                .u32("addr", e.addr)
                .u32("count", e.count)
                .u32("tc", e.tc)
                .u32("pos", e.pos)
                .u32("tc_pos", e.tc_pos)
                .u64("frames", e.frames)
                .u64("age_ns", if e.active { (now - e.anchor).max(0) as u64 } else { 0 })
                .bool("active", e.active)
                .build()
        });
        StateTable::new()
            .bool("ac97", i.variant == Variant::Ac97)
            .u8s("ccs", &s.ccs)
            .u8s("mt", &s.mt)
            .u8("pm_state", s.pm_state)
            .u16("status_errors", s.status_errors)
            .u8s("eeprom", &s.eeprom)
            .value("eng", toml::Value::Array(engines.collect()))
            .u32("rate", s.rate)
            .u8s("frame_bits", &s.frame_bits)
            .u16s("frame_word", &s.frame_word)
            .u8s("ak4381", &s.ak4381)
            .u8s("ak4355", &s.ak4355)
            .u16s("ac97_regs", &s.ac97)
            .u64("analog_frames", s.stats.analog_frames)
            .u32("analog_last", s.stats.analog_last)
            .u64("spdif_frames", s.stats.spdif_frames)
            .u64("capture_frames", s.stats.capture_frames)
            .u64("dropped_frames", s.stats.dropped_frames)
            .u32("ak4381_writes", s.stats.ak4381_writes)
            .u32("ak4355_writes", s.stats.ak4355_writes)
            .u32("codec_rejects", s.stats.codec_rejects)
            .build()
    }

    /// Called with the card stopped.
    fn load_state(&self, v: &toml::Value) -> Result<(), String> {
        let i = &self.inner;
        if field_bool(v, "ac97")? != (i.variant == Variant::Ac97) {
            return Err("dm8: saved for the other variant".into());
        }
        let now = i.now();
        let saved = field(v, "eng")?.as_array().ok_or("eng: not a list")?;
        if saved.len() != ENGINES {
            return Err(format!("eng: {} engines, expected {ENGINES}", saved.len()));
        }
        let mut eng = [Engine::default(); ENGINES];
        for (e, t) in eng.iter_mut().zip(saved) {
            // The clock anchor is the card's clock now, less the engine's
            // age when it was saved.
            *e = Engine {
                addr: field_u32(t, "addr")?,
                count: field_u32(t, "count")?,
                tc: field_u32(t, "tc")?,
                pos: field_u32(t, "pos")?,
                tc_pos: field_u32(t, "tc_pos")?,
                frames: field_u64(t, "frames")?,
                anchor: now - field_u64(t, "age_ns")? as i64,
                active: field_bool(t, "active")?,
            };
        }
        let mut ac97 = [0u16; AC97_REGS];
        load_u16_slice(field(v, "ac97_regs")?, &mut ac97);
        let mut frame_word = [0u16; 2];
        load_u16_slice(field(v, "frame_word")?, &mut frame_word);
        let mut s = i.state.lock();
        let loaded = State {
            ccs: field_u8s(v, "ccs")?,
            mt: field_u8s(v, "mt")?,
            pm_state: field_u8(v, "pm_state")?,
            status_errors: field_u16(v, "status_errors")?,
            irq_level: s.irq_level,
            eeprom: field_u8s(v, "eeprom")?,
            eng,
            rate: field_u32(v, "rate")?,
            gpio_pins: 0,
            frame_bits: field_u8s(v, "frame_bits")?,
            frame_word,
            ak4381: field_u8s(v, "ak4381")?,
            ak4355: field_u8s(v, "ak4355")?,
            ac97,
            stats: Stats {
                analog_frames: field_u64(v, "analog_frames")?,
                analog_last: field_u32(v, "analog_last")?,
                spdif_frames: field_u64(v, "spdif_frames")?,
                capture_frames: field_u64(v, "capture_frames")?,
                dropped_frames: field_u64(v, "dropped_frames")?,
                ak4381_writes: field_u32(v, "ak4381_writes")?,
                ak4355_writes: field_u32(v, "ak4355_writes")?,
                codec_rejects: field_u32(v, "codec_rejects")?,
            },
            timer: None,
            timer_gen: s.timer_gen + 1,
            timer_due: 0,
            scratch: std::mem::take(&mut s.scratch),
        };
        loaded.check()?;
        if s.timer.is_some() {
            return Err("dm8 loaded while its timer runs".into());
        }
        *s = loaded;
        i.capture_active.store(s.eng[ENG_RDMA0].active, Ordering::Relaxed);
        s.gpio_pins = s.pins();
        i.update_irq(&mut s);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
