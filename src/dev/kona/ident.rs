//! What the pipe says it is: the widget id register, the one-wire identity
//! record, the two words the driver's attach classes the pipe by, and the
//! configuration block the host interface processor answers a configuration
//! request with.
//!
//! The configuration is an InfiniteReality2 of the Onyx2 deskside: a GE14-4
//! geometry board, one or two RM7 raster managers each with a TM7 texture
//! board of 64 MB, and a DG5 display generator of eight channels. The deskside
//! has no second host-interface record, so the one-wire chain holds the
//! geometry board's alone. Part numbers, names and revisions are those of QEMU's model
//! of that pipe; ROM serials and barcodes are synthetic and marked so (the
//! serial's top 32 bits are the marker 0x51454d55 and the barcode starts
//! "EMU"; both follow QEMU's synthetic-serial scheme).
//!
//! Ported from hw/misc/sgi-kona.c in jamesbraid/qemu-sgi-origami.

use super::consts::*;
use crate::dev::w1::sgi::BoardRecord;
use crate::dev::w1::{slot, Bus, Ds2502};

/// One board of the configuration: the name, part number and revision its
/// one-wire record carries. A field QEMU's model leaves blank is empty.
#[derive(Clone, Copy)]
struct BoardSpec {
    name: &'static str,
    part: &'static str,
    revision: &'static str,
}

const GEOMETRY: BoardSpec = BoardSpec { name: "GE14-4", part: "030-1129-002", revision: "F" };
const RASTER: BoardSpec = BoardSpec { name: "RM7", part: "030-1054-001", revision: "G" };
const TEXTURE: BoardSpec = BoardSpec { name: "TM7", part: "030-1053-001", revision: "F" };
const DISPLAY: BoardSpec = BoardSpec { name: "", part: "030-1087-001", revision: "" };

/// Geometry engines on the GE14-4.
const ENGINES: u32 = 4;
const OCCUPANCY: u32 = 0xf;
/// Display generator channels, and external gamma RAM: the deskside's DG.
const CHANNELS: u32 = 8;
const EXTERNAL_GAMMA: u32 = 1;
/// The size code for 64 MB of texture memory per RM.
const TEXTURE_CODE: u32 = 2;
const TEXTURE_MB: u32 = 64;
/// Chip identity words of QEMU's model of the deskside pipe.
const BOARD_WORD: u32 = 0x8016_0a04;
const RASTER_WORD: u32 = 0x0302_0c06;
const TRAILING_WORD: u32 = 0x51bf_1002;
/// The board set the driver derives for an InfiniteReality2.
const BOARD_SET: u32 = 1;
/// "4MB external" FIFO RAM, as QEMU's model reports.
const FIFO_RAM_4MB_EXTERNAL: u32 = 1;

/// The configuration a pipe presents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KonaConfig {
    /// Raster managers: the deskside holds one or two.
    pub rm_count: u32,
    /// Which pipe slot the machine puts this on, 0 to 3: each slot's
    /// records take sixteen synthetic serial instances.
    pub identity_index: u8,
    /// The widget the pipe's DMA goes to out of reset: the host node's
    /// crossbar widget, 0 to 15. The register's real power-on value is
    /// unknown. The model starts it here so DMA works before software has
    /// programmed it; QEMU's model does the same with its board's hub widget.
    pub host_widget: u8,
}

impl KonaConfig {
    /// An InfiniteReality2 with `rm_count` RM7 boards in pipe slot 0.
    pub fn ir2(rm_count: u32) -> Result<Self, String> {
        if rm_count != 1 && rm_count != 2 {
            return Err(format!(
                "kona: rm-count {rm_count}: an Onyx2 deskside pipe holds one or two raster managers"
            ));
        }
        Ok(KonaConfig { rm_count, identity_index: 0, host_widget: 0 })
    }

    pub fn in_slot(mut self, index: u8) -> Result<Self, String> {
        if index >= 4 {
            return Err(format!("kona: identity index {index}: each pipe slot's records use sixteen serial instances, 0 to 3"));
        }
        self.identity_index = index;
        Ok(self)
    }

    /// The widget the pipe's DMA goes to out of reset (bits 25:22 of the
    /// widget control register).
    pub fn with_host_widget(mut self, widget: u8) -> Result<Self, String> {
        if widget > 15 {
            return Err(format!("kona: host widget {widget}: a widget number is four bits"));
        }
        self.host_widget = widget;
        Ok(self)
    }

    /// The widget control register's value out of reset.
    pub(super) fn widget_control_reset(&self) -> u32 {
        (self.host_widget as u32) << 22
    }

    /// Texture memory on each raster manager's texture board, in bytes.
    pub fn texture_bytes_per_rm(&self) -> u64 {
        (TEXTURE_MB as u64) << 20
    }

    fn base(&self) -> u32 {
        16 * self.identity_index as u32
    }
}

impl Default for KonaConfig {
    fn default() -> Self {
        KonaConfig { rm_count: 2, identity_index: 0, host_widget: 0 }
    }
}

/// The widget id register: presence bit, manufacturer, part, revision.
pub(super) fn widget_id(revision: u32) -> u32 {
    (revision << WIDGET_ID_REVISION_SHIFT)
        | (PART << WIDGET_ID_PART_SHIFT)
        | (MANUFACTURER << WIDGET_ID_MANUFACTURER_SHIFT)
        | WIDGET_ID_ALWAYS_ONE
}

/// The synthetic one-wire ROM serial of component `letter`, instance `n`:
/// the marker 0x51454d55, the letter, the instance.
pub(super) fn synthetic_serial(letter: u8, n: u32) -> u64 {
    0x5145_4d55u64 << 16 | (letter as u64) << 8 | n as u64
}

/// The barcode a board record carries: the marker, the letter and the
/// instance in decimal, as short as the instance allows (six characters
/// below 100).
fn barcode(letter: char, n: u32) -> String {
    let width = if n < 100 { 6 } else if n < 1000 { 7 } else { 8 };
    format!("EMU{letter}{n:0w$}", w = width - 4)
}

/// The identity bus: the geometry board's record on a DS2502.
pub(super) struct Identity {
    pub bus: Bus,
}

impl Identity {
    pub fn new(cfg: &KonaConfig) -> Self {
        let n = cfg.base();
        let record = BoardRecord {
            barcode: &barcode('G', n),
            part: GEOMETRY.part,
            revision: GEOMETRY.revision,
            name: GEOMETRY.name,
        };
        let mut bus = Bus::new();
        bus.add(Box::new(Ds2502::new(ONEWIRE_FAMILY, synthetic_serial(b'G', n), &record.image())));
        Identity { bus }
    }

    /// The one-wire register after a write of `value`: the slot the write
    /// started, completed in the same access with DONE set and LEVEL the
    /// sampled wire. The pulse and sample fields the guest wrote persist.
    pub fn write(&mut self, value: u32) -> u32 {
        let (pulse, sample) = slot::fields(value as u64);
        let level = slot::slot(&mut self.bus, pulse, sample);
        (value & ONEWIRE_WRITABLE) | ONEWIRE_DONE | if level { ONEWIRE_LEVEL } else { 0 }
    }
}

/// The two words the driver's attach reads through the indirect pair to
/// class the pipe: after selecting `SELECT_PRODUCT` the low bits are the
/// product's (0 for an InfiniteReality2), after `SELECT_REDUCED` bit 10 would
/// make it a Reality (0).
pub(super) fn class_word(selector: u32) -> Option<u32> {
    matches!(selector, SELECT_PRODUCT | SELECT_REDUCED).then_some(0)
}

fn put(block: &mut [u8], at: usize, word: u32) {
    block[at..at + 4].copy_from_slice(&word.to_be_bytes());
}

fn put_words(block: &mut [u8], at: usize, count: usize, word: u32) {
    for i in 0..count {
        put(block, at + 4 * i, word);
    }
}

/// `text` right-justified in a field of `width` spaces.
fn put_text(field: &mut [u8], text: &str) {
    let width = field.len();
    let len = text.len().min(width);
    field.fill(b' ');
    field[width - len..].copy_from_slice(&text.as_bytes()[..len]);
}

/// One 0x4c-byte board record in the layout the driver's one-wire reader
/// builds and the board report prints: flags (0, both pages present), the six-byte
/// one-wire number, family 0x0b, a good CRC, page format 1, then barcode,
/// part, revision, the three flag bytes and the name.
fn put_record(rec: &mut [u8], b: &BoardSpec, instance: u32) {
    let serial = synthetic_serial(b'G', instance);
    for i in 0..6 {
        rec[0x04 + i] = (serial >> (40 - 8 * i)) as u8;
    }
    rec[0x0a] = 0x0b;
    rec[0x0b] = 1;
    rec[0x0c] = 1;
    put_text(&mut rec[0x0d..0x17], &barcode('G', instance));
    put_text(&mut rec[0x17..0x2a], b.part);
    rec[0x2c..0x32].fill(b' ');
    put_text(&mut rec[0x32..0x36], b.revision);
    rec[0x36..0x3c].fill(0xff);
    put_text(&mut rec[0x3c..0x4a], b.name);
}

// Offsets of the configuration block's fields, from its start.
const CFG_BOARD_SET: usize = 0x00;
const CFG_ENGINES: usize = 0x04;
const CFG_RMS: usize = 0x08;
const CFG_FIFO_RAM: usize = 0x0c;
const CFG_CHANNELS: usize = 0x14;
const CFG_VIDEO_OPTION: usize = 0x18;
const CFG_GAMMA: usize = 0x20;
const CFG_RM_PRESENT: usize = 0x24;
const CFG_RM_TEXTURE: usize = 0x28;
const CFG_DG_CHANNEL_COUNT: usize = 0x44;
const CFG_ACTIVE_ENGINES: usize = 0x54;
const CFG_ACTIVE_RMS: usize = 0x58;
const CFG_OCCUPANCY: usize = 0x5c;
const CFG_WIDTH: usize = 0x84;
const CFG_HEIGHT: usize = 0x88;
const CFG_PIXEL_DEPTH: usize = 0x8c;
const CFG_POOLS: usize = 0x90;
const CFG_BOARD_WORD: usize = 0x100;
const CFG_GE_WORDS: usize = 0x104;
const CFG_RM_CHIPS: usize = 0x114;
const CFG_TRAILING_WORD: usize = 0x344;
const CFG_DG_WORDS: usize = 0x348;
const CFG_DG_CHANNELS: usize = 0x35c;
const CFG_NIC_RM: usize = 0x3e8;
const CFG_NIC_TM: usize = 0x564;
const CFG_NIC_DG: usize = 0x6e0;

/// The block's size in bytes, and where the answer puts it on the return
/// page.
pub(super) const CONFIG_SIZE: usize = 0x778;
pub(super) const CONFIG_OFFSET: u32 = 8;

/// The four pipe-memory pools the driver's allocator checks requests
/// against: the co-processor, the shared display lists, the geometry
/// engines and texture memory. No source gives the raw words, so these are
/// the model's own choice inside the kernel's limits: the granule is
/// non-zero and divides every alignment requested, alignment over granule
/// stays below 256, and the geometry class takes keys to 10 and is not
/// shared.
struct Pool {
    max_key: u32,
    max_align: u32,
    copies: u32,
    shared: u32,
    copy_down: u32,
    granule: u32,
    capacity: u32,
}

fn pools(cfg: &KonaConfig) -> [Pool; 4] {
    let _ = cfg;
    [
        Pool { max_key: 0, max_align: 0x4000, copies: 1, shared: 0, copy_down: 1, granule: 0x400, capacity: 0x100 },
        Pool { max_key: 0, max_align: 0x4000, copies: 1, shared: 1, copy_down: 1, granule: 0x400, capacity: 0x3c00 },
        Pool { max_key: 10, max_align: 0x4000, copies: ENGINES, shared: 0, copy_down: 1, granule: 0x400, capacity: 0x400 },
        Pool { max_key: 0, max_align: 0x4000, copies: 1, shared: 0, copy_down: 1, granule: 0x400, capacity: TEXTURE_MB * (1 << 20) / 0x400 },
    ]
}

/// The configuration block the pipe answers a configuration request with:
/// every field at the offset the driver's board report reads it from.
/// `width` and `height` are the managed area's, 1280x1024 until a video
/// format combination sets up another.
pub(super) fn config_block(cfg: &KonaConfig, width: u32, height: u32) -> Vec<u8> {
    let mut b = vec![0u8; CONFIG_SIZE];
    let base = cfg.base();

    put(&mut b, CFG_BOARD_SET, BOARD_SET);
    put(&mut b, CFG_ENGINES, ENGINES);
    put(&mut b, CFG_RMS, cfg.rm_count);
    put(&mut b, CFG_FIFO_RAM, FIFO_RAM_4MB_EXTERNAL);
    put(&mut b, CFG_CHANNELS, CHANNELS);
    put(&mut b, CFG_VIDEO_OPTION, 0);
    put(&mut b, CFG_GAMMA, EXTERNAL_GAMMA);
    // The display generator's own channel count, which the combination
    // checker compares a combination's enabled channels against.
    put(&mut b, CFG_DG_CHANNEL_COUNT, CHANNELS);
    put(&mut b, CFG_ACTIVE_ENGINES, ENGINES);
    put(&mut b, CFG_ACTIVE_RMS, cfg.rm_count);
    put(&mut b, CFG_OCCUPANCY, OCCUPANCY);
    put(&mut b, CFG_WIDTH, width);
    put(&mut b, CFG_HEIGHT, height);
    // Small pixel depth.
    put(&mut b, CFG_PIXEL_DEPTH, 0);

    for (i, p) in pools(cfg).iter().enumerate() {
        let at = CFG_POOLS + 0x1c * i;
        for (k, w) in [p.max_key, p.max_align, p.copies, p.shared, p.copy_down, p.granule, p.capacity]
            .into_iter()
            .enumerate()
        {
            put(&mut b, at + 4 * k, w);
        }
    }

    put(&mut b, CFG_BOARD_WORD, BOARD_WORD);
    for (i, w) in [0, 0x1483_2057, 0x2473_1057, 0x1423_1057].into_iter().enumerate() {
        put(&mut b, CFG_GE_WORDS + 4 * i, w);
    }
    for i in 0..cfg.rm_count as usize {
        let chips = CFG_RM_CHIPS + 0x8c * i;
        put(&mut b, CFG_RM_PRESENT + 8 * i, 1);
        put(&mut b, CFG_RM_TEXTURE + 8 * i, TEXTURE_CODE);
        put(&mut b, chips, RASTER_WORD);
        put(&mut b, chips + 0x04, 0x1503_2057);
        put(&mut b, chips + 0x08, 0x1543_1057);
        put_words(&mut b, chips + 0x0c, 4, 0x2463_1057);
        put_words(&mut b, chips + 0x1c, 8, 0x24d3_1057);
        put_words(&mut b, chips + 0x3c, 20, 0x5493_8057);
        put_record(&mut b[CFG_NIC_RM + 0x4c * i..][..0x4c], &RASTER, base + 2 + i as u32);
        put_record(&mut b[CFG_NIC_TM + 0x4c * i..][..0x4c], &TEXTURE, base + 6 + i as u32);
    }
    put(&mut b, CFG_TRAILING_WORD, TRAILING_WORD);
    put(&mut b, CFG_DG_WORDS, 0x0553_2057);
    put_words(&mut b, CFG_DG_WORDS + 4, 4, 0x1583_8057);
    put_words(&mut b, CFG_DG_CHANNELS, CHANNELS as usize, 0x1563_1057);
    put_record(&mut b[CFG_NIC_DG..][..0x4c], &DISPLAY, base + 10);
    b
}

#[cfg(test)]
pub(in crate::dev::kona) mod tests {
    use super::*;

    #[test]
    fn the_widget_id_composes_revision_part_manufacturer_and_presence() {
        assert_eq!(widget_id(0), 0x0c10_2000 | (0x2aa << 1) | 1);
        assert_eq!(widget_id(3) >> 28, 3);
    }

    #[test]
    fn barcodes_lengthen_with_the_instance() {
        assert_eq!(barcode('G', 0), "EMUG00");
        assert_eq!(barcode('G', 63), "EMUG63");
        assert_eq!(barcode('G', 123), "EMUG123");
    }

    #[test]
    fn a_deskside_holds_one_or_two_raster_managers() {
        assert!(KonaConfig::ir2(1).is_ok());
        assert!(KonaConfig::ir2(2).is_ok());
        assert!(KonaConfig::ir2(4).is_err());
        assert!(KonaConfig::ir2(0).is_err());
    }

    #[test]
    fn the_host_widget_sets_the_widget_control_reset_value() {
        assert_eq!(KonaConfig::default().widget_control_reset(), 0);
        let cfg = KonaConfig::default().with_host_widget(9).unwrap();
        assert_eq!(cfg.widget_control_reset(), 9 << 22);
        assert!(KonaConfig::default().with_host_widget(16).is_err());
    }

    const CONFIG_SIZE: usize = 0x778;

    pub(in crate::dev::kona) struct Board {
        pub(in crate::dev::kona) name: &'static str,
        pub(in crate::dev::kona) part: &'static str,
        pub(in crate::dev::kona) revision: &'static str,
    }

    /// QEMU's deskside pipe: GE14-4 030-1129-002, RM7 030-1054-001 and TM
    /// 030-1053-001 with 64 MB each, DG 030-1087-001 reporting eight channels and
    /// 64K of external gamma; brd "80160a04 3020c06/3020c06/-/- 51bf1002".
    pub(in crate::dev::kona) struct Expected {
        pub(in crate::dev::kona) rms: u32,
        pub(in crate::dev::kona) rm: Board,
        pub(in crate::dev::kona) tm: Board,
        pub(in crate::dev::kona) dg: Board,
    }

    pub(in crate::dev::kona) const DESKSIDE: Expected = Expected {
        rms: 2,
        rm: Board { name: "RM7", part: "030-1054-001", revision: "G" },
        tm: Board { name: "TM7", part: "030-1053-001", revision: "F" },
        dg: Board { name: "", part: "030-1087-001", revision: "" },
    };

    pub(in crate::dev::kona) fn synthetic_serial(instance: u32) -> u64 {
        0x5145_4d55u64 << 16 | (b'G' as u64) << 8 | instance as u64
    }

    pub(in crate::dev::kona) fn put_text(field: &mut [u8], text: &str) {
        assert!(text.len() <= field.len());
        field.fill(b' ');
        let at = field.len() - text.len();
        field[at..].copy_from_slice(text.as_bytes());
    }

    /// The board report's layout, the 0x4c-byte record at `rec`.
    pub(in crate::dev::kona) fn put_record(rec: &mut [u8], b: &Board, instance: u32) {
        let serial = synthetic_serial(instance);
        for i in 0..6 {
            rec[4 + i] = (serial >> (40 - 8 * i)) as u8;
        }
        rec[0x0a] = 0x0b;
        rec[0x0b] = 1;
        rec[0x0c] = 1;
        put_text(&mut rec[0x0d..0x17], &format!("EMUG{instance:02}"));
        put_text(&mut rec[0x17..0x2a], b.part);
        rec[0x2c..0x32].fill(b' ');
        put_text(&mut rec[0x32..0x36], b.revision);
        rec[0x36..0x3c].fill(0xff);
        put_text(&mut rec[0x3c..0x4a], b.name);
    }

    pub(in crate::dev::kona) fn put(b: &mut [u8], at: usize, w: u32) {
        b[at..at + 4].copy_from_slice(&w.to_be_bytes());
    }

    pub(in crate::dev::kona) fn put_words(b: &mut [u8], at: usize, count: usize, w: u32) {
        for i in 0..count {
            put(b, at + 4 * i, w);
        }
    }

    /// The block as QEMU's values lay it out, written independently of the
    /// model's builder: every field at the offset the board report reads it from.
    pub(in crate::dev::kona) fn expected_block(e: &Expected) -> Vec<u8> {
        // The pool descriptors, two sized by the configuration.
        const POOLS: [[u32; 7]; 4] = [
            [0, 0x4000, 1, 0, 1, 0x400, 0x100],
            [0, 0x4000, 1, 1, 1, 0x400, 0x3c00],
            [10, 0x4000, 4, 0, 1, 0x400, 0x400],
            [0, 0x4000, 1, 0, 1, 0x400, 0x10000],
        ];
        let mut b = vec![0u8; CONFIG_SIZE];
        put(&mut b, 0x00, 1);
        put(&mut b, 0x04, 4);
        put(&mut b, 0x08, e.rms);
        put(&mut b, 0x0c, 1);
        put(&mut b, 0x14, 8);
        put(&mut b, 0x18, 0);
        put(&mut b, 0x20, 1);
        // The display generator's own channel count, which setmon checks.
        put(&mut b, 0x44, 8);
        put(&mut b, 0x54, 4);
        put(&mut b, 0x58, e.rms);
        put(&mut b, 0x5c, 0xf);
        put(&mut b, 0x84, 1280);
        put(&mut b, 0x88, 1024);
        put(&mut b, 0x8c, 0);
        for (c, pool) in POOLS.iter().enumerate() {
            for (w, v) in pool.iter().enumerate() {
                put(&mut b, 0x90 + 0x1c * c + 4 * w, *v);
            }
        }
        put(&mut b, 0x90 + 0x1c * 2 + 8, 4);
        put(&mut b, 0x90 + 0x1c * 3 + 0x18, 64 * 1024);

        // The chip identity lines of the board report.
        put(&mut b, 0x100, 0x80160a04);
        put(&mut b, 0x108, 0x14832057);
        put(&mut b, 0x10c, 0x24731057);
        put(&mut b, 0x110, 0x14231057);
        for i in 0..e.rms as usize {
            let rm = 0x114 + 0x8c * i;
            put(&mut b, 0x24 + 8 * i, 1);
            put(&mut b, 0x28 + 8 * i, 2);
            put(&mut b, rm, 0x3020c06);
            put(&mut b, rm + 4, 0x15032057);
            put(&mut b, rm + 8, 0x15431057);
            put_words(&mut b, rm + 0x0c, 4, 0x24631057);
            put_words(&mut b, rm + 0x1c, 8, 0x24d31057);
            put_words(&mut b, rm + 0x3c, 20, 0x54938057);
            put_record(&mut b[0x3e8 + 0x4c * i..][..0x4c], &e.rm, 2 + i as u32);
            put_record(&mut b[0x564 + 0x4c * i..][..0x4c], &e.tm, 6 + i as u32);
        }
        put(&mut b, 0x344, 0x51bf1002);
        put(&mut b, 0x348, 0x05532057);
        put_words(&mut b, 0x34c, 4, 0x15838057);
        put_words(&mut b, 0x35c, 8, 0x15631057);
        put_record(&mut b[0x6e0..][..0x4c], &e.dg, 10);
        b
    }

    /// QEMU: ir2-ge14-4. The block the pipe builds is the deskside's byte for
    /// byte, against a block laid out here independently of the builder, for
    /// two raster managers and for one.
    #[test]
    fn the_configuration_block_matches_the_deskside_layout() {
        for rms in [2, 1] {
            let cfg = KonaConfig::ir2(rms).unwrap();
            let block = config_block(&cfg, 1280, 1024);
            let want = expected_block(&Expected { rms, ..DESKSIDE });
            assert_eq!(block.len(), CONFIG_SIZE);
            if let Some(i) = (0..CONFIG_SIZE).step_by(4).find(|&i| block[i..i + 4] != want[i..i + 4]) {
                panic!("{rms} RMs: first difference at {i:#x}: {:x?} against {:x?}", &block[i..i + 4], &want[i..i + 4]);
            }
        }
    }

    #[test]
    fn the_managed_size_is_reported() {
        let block = config_block(&KonaConfig::default(), 640, 480);
        assert_eq!(u32::from_be_bytes(block[0x84..0x88].try_into().unwrap()), 640);
        assert_eq!(u32::from_be_bytes(block[0x88..0x8c].try_into().unwrap()), 480);
    }
}
