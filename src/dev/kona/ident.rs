//! What the pipe says it is: the widget id register, the one-wire identity
//! record, the two words the driver's attach classes the pipe by, and the
//! configuration block the host interface processor answers a configuration
//! request with.
//!
//! The configuration is a product of the table below, the InfiniteReality2
//! of the Onyx2 deskside by default: a GE14-4 geometry board, one or two RM7
//! raster managers each with a TM7 texture board of 64 MB, and a DG5 display
//! generator of eight channels. The InfiniteReality2E and InfiniteReality3
//! change the boards, the board set and the texture size. A rack or Onyx 3400
//! pipe adds a host board (`HostBoard`), which allows four raster managers
//! and makes the DG5-2 the default display generator. The deskside has no
//! second host-interface record, so the one-wire chain holds the geometry
//! board's alone. Part numbers, names and revisions are those of the ported model of
//! that pipe, or the capture a product's row names; ROM serials and barcodes
//! are synthetic and marked so (the serial's top 32 bits are the marker
//! 0x51454d55 and the barcode starts "EMU"; both follow the ported
//! synthetic-serial scheme).
//!
//! Ported from hw/misc/sgi-kona.c in jamesbraid/qemu-sgi-origami.

use super::defs::*;
use crate::dev::w1::sgi::BoardRecord;
use crate::dev::w1::{slot, Bus, Ds2502};

/// One board of the configuration: the name, part number and revision its
/// one-wire record carries. A field the ported model leaves blank is empty.
#[derive(Clone, Copy)]
pub(super) struct BoardSpec {
    name: &'static str,
    part: &'static str,
    revision: &'static str,
}

/// Display generator channels: the DG5-8's.
pub(super) const CHANNELS: u32 = 8;

/// What a display generator board reports about itself. The identity word the
/// configuration block ends its raster group with is built from these: the high
/// byte and the 0x1002 low half-word are constant for a DG5 board (inferred
/// from the captures), bits 18 to 16 are the channel count less one, 21 to 19
/// the display option, bit 22 the colour-map size and bit 23 external gamma.
///
/// Limit: the display model's channel arrays are eight long, so a board with
/// fewer channels answers for its own and refuses the rest. The DG5-2 with a
/// GVO option is not presented: its option channel is not modelled. Its word,
/// as the Onyx2 rack capture reads it, is checked in a test.
pub(super) struct Display {
    pub channels: u32,
    /// 3 TVO, 4 DPLEX, 5 DVP, 6 GVO, 7 none.
    pub option: u32,
    pub colour_map: bool,
    pub external_gamma: bool,
    /// Bits 31 to 24 of the identity word.
    high: u32,
    pub board: BoardSpec,
}

impl Display {
    pub(super) fn word(&self) -> u32 {
        (self.high << 24)
            | (self.external_gamma as u32) << 23
            | (self.colour_map as u32) << 22
            | self.option << 19
            | (self.channels - 1) << 16
            | 0x1002
    }
}

/// The DG5-8 of the deskside capture: eight channels, external gamma, no
/// display option and so no option record.
pub(super) const DG5_8: Display = Display {
    channels: 8,
    option: 7,
    colour_map: false,
    external_gamma: true,
    high: 0x51,
    board: BoardSpec { name: "", part: "030-1087-001", revision: "" },
};
/// The DG5-2 on its own: the rack capture's base board (high byte and part family) with the
/// part a published PCA list gives a DG5-2, and no option. No capture shows it bare, an
/// approximation.
pub(super) const DG5_2: Display = Display {
    channels: 2,
    option: 7,
    colour_map: false,
    external_gamma: false,
    high: 0xf9,
    board: BoardSpec { name: "DG5-2", part: "030-1055-001", revision: "" },
};

/// The display generators a pipe takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayBoard {
    Dg5_8,
    Dg5_2,
}

impl DisplayBoard {
    pub fn name(self) -> &'static str {
        match self {
            DisplayBoard::Dg5_8 => "dg5-8",
            DisplayBoard::Dg5_2 => "dg5-2",
        }
    }

    pub(super) fn spec(self) -> &'static Display {
        match self {
            DisplayBoard::Dg5_8 => &DG5_8,
            DisplayBoard::Dg5_2 => &DG5_2,
        }
    }
}

/// The geometry boards a pipe takes: its identity record, engines, and the board word the
/// configuration block carries.
pub(super) struct GeometrySpec {
    id: &'static str,
    board: BoardSpec,
    engines: u32,
    occupancy: u32,
    board_word: u32,
}

/// The deskside capture's GE14-4, with its hinv revision.
const GE14_4: GeometrySpec = GeometrySpec { id: "ge14-4", board: BoardSpec { name: "GE14-4", part: "030-1129-002", revision: "F" }, engines: 4, occupancy: 0xf, board_word: 0x8016_0a04 };
/// An owner's board list and gfxinfo for a two-engine pipe: part 030-1051-002 rev K, two engines,
/// occupancy 0x03. Its board words are the GE14-4's, an approximation.
const GE14_2: GeometrySpec = GeometrySpec { id: "ge14-2", board: BoardSpec { name: "GE14-2", part: "030-1051-002", revision: "K" }, engines: 2, occupancy: 0x3, board_word: 0x8016_0a04 };
/// The rack capture's GE16-4, with its hinv revision and words.
const GE16_4: GeometrySpec = GeometrySpec { id: "ge16-4", board: BoardSpec { name: "GE16-4", part: "030-1398-001", revision: "E" }, engines: 4, occupancy: 0xf, board_word: GE16_BOARD_WORD };

/// The geometry boards of the model, by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Geometry {
    Ge14_4,
    Ge14_2,
    Ge16_4,
}

impl Geometry {
    pub(super) fn spec(self) -> &'static GeometrySpec {
        match self {
            Geometry::Ge14_4 => &GE14_4,
            Geometry::Ge14_2 => &GE14_2,
            Geometry::Ge16_4 => &GE16_4,
        }
    }
}

/// How much of a pixel the pipe keeps: what the configuration block reports and the X server
/// chooses its visuals by. Large and Extra-Large are not modelled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PixelDepth {
    #[default]
    Small,
    Medium,
}

impl PixelDepth {
    /// The pixel bytes the X server's check wants of the raster managers' budget.
    fn need(self) -> u64 {
        match self {
            PixelDepth::Small => 8,
            PixelDepth::Medium => 16,
        }
    }
}

/// The board a pipe's host link ends in, which decides how many raster managers it holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HostBoard {
    /// The Onyx2 deskside: no host board, at most two raster managers.
    #[default]
    Deskside,
    /// An Onyx2 rack's Ktown.
    Ktown,
    /// An Onyx 3400's Ktown2, which shows no record of its own to the model.
    Ktown2,
}

/// "4MB external" FIFO RAM, as the ported model reports.
const FIFO_RAM_4MB_EXTERNAL: u32 = 1;

/// The chassis a pipe fits in. Iris builds the Onyx2 deskside alone; the
/// others are recorded so a row says where its product shipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Host {
    Onyx2Deskside,
    Onyx2Rack,
    Onyx300,
    Onyx3000,
}

impl Host {
    pub fn name(self) -> &'static str {
        match self {
            Host::Onyx2Deskside => "Onyx2 deskside",
            Host::Onyx2Rack => "Onyx2 rack",
            Host::Onyx300 => "Onyx 300",
            Host::Onyx3000 => "Onyx 3000",
        }
    }
}

/// The products of the pipe the model presents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Product {
    Ir2,
    Ir2e,
    Ir3,
}

/// One row of the product table: everything the model reports that
/// differs between products, as data. A field the model does not
/// present differently stays a shared constant above.
pub struct ProductSpec {
    pub product: Product,
    /// The name `[graphics] board` and a recording's board record use.
    pub name: &'static str,
    /// The name the guest's `hinv` prints for the board.
    pub label: &'static str,
    /// The board-set word at the start of the configuration block.
    board_set: u32,
    /// What the pipe answers when the driver's attach reads the product
    /// selector (`SELECT_PRODUCT`): the low bits class the pipe.
    class_word: u32,
    /// The geometry boards the product takes, the default first.
    geometries: &'static [&'static GeometrySpec],
    raster: BoardSpec,
    texture: BoardSpec,
    /// Texture memory on each raster manager's texture board, and the size
    /// code the block carries for it.
    texture_mb: u32,
    texture_code: u32,
    /// The raster manager's identity word.
    raster_word: u32,
    /// Where the product shipped.
    pub hosts: &'static [Host],
}

impl ProductSpec {
    /// Texture memory on each raster manager's texture board, in MB.
    pub fn texture_mb(&self) -> u32 {
        self.texture_mb
    }

    pub fn fits(&self, host: Host) -> bool {
        self.hosts.contains(&host)
    }

    /// Whether the product may go in `host`, or why not. A chassis the table
    /// leaves out is one no manual or capture places the product in.
    pub fn check_host(&self, host: Host) -> Result<(), String> {
        if self.fits(host) {
            return Ok(());
        }
        let shipped: Vec<_> = self.hosts.iter().map(|h| h.name()).collect();
        Err(format!(
            "{} is not modelled in the {}: no manual or capture places it there (it fits the {})",
            self.label,
            host.name(),
            shipped.join(", ")
        ))
    }
}

/// The product table, in the order of [`Product`].
pub const PRODUCTS: [ProductSpec; 3] = [
    // The Onyx2 deskside's InfiniteReality2 (GE14-4 with RM7 and TM7): the
    // deskside capture's values.
    ProductSpec {
        product: Product::Ir2,
        name: "ir2",
        label: "InfiniteReality2",
        board_set: 1,
        class_word: 0,
        geometries: &[&GE14_4, &GE14_2],
        raster: BoardSpec { name: "RM7", part: "030-1054-001", revision: "G" },
        texture: BoardSpec { name: "TM7", part: "030-1053-001", revision: "F" },
        texture_mb: 64,
        texture_code: 2,
        raster_word: IR2_RASTER_WORD,
        hosts: &[Host::Onyx2Deskside],
    },
    // InfiniteReality2E: the GE16-4 geometry board with RM9 raster managers
    // and the TM7-64 texture board. The attach classes a pipe whose geometry
    // board's record holds "GE16-" as set 4 (established). The raster slot
    // word is the InfiniteReality2's, type 6 with texture code 2: type 0xb
    // would make the pipe present as an InfiniteReality3. The GE16-4 and its
    // identity word are the rack capture's, and the geometry word is inferred;
    // the RM9 and TM7 part numbers are established for the 2E, their
    // revisions and the chip words in the block are unknown (the chip words
    // are the InfiniteReality2's).
    ProductSpec {
        product: Product::Ir2e,
        name: "ir2e",
        label: "InfiniteReality2E",
        board_set: 4,
        class_word: 0,
        geometries: &[&GE16_4],
        raster: BoardSpec { revision: "", ..RM9 },
        texture: BoardSpec { name: "TM7", part: "030-1053-001", revision: "" },
        texture_mb: 64,
        texture_code: 2,
        raster_word: IR2_RASTER_WORD,
        hosts: &[Host::Onyx2Deskside, Host::Onyx2Rack, Host::Onyx300],
    },
    // InfiniteReality3: the GE16-4 and RM9 records with a 256 MB TM8 texture
    // board per raster manager, as the rack capture presents them (each
    // raster manager holds its own copy, inferred). The TM8's name is
    // inferred from the board report's pairing and its revision is unknown.
    // The attach classes the pipe by the product word reading 1.
    ProductSpec {
        product: Product::Ir3,
        name: "ir3",
        label: "InfiniteReality3",
        board_set: 5,
        class_word: 1,
        geometries: &[&GE16_4],
        raster: RM9,
        texture: BoardSpec { name: "TM8", part: "030-1588-001", revision: "" },
        texture_mb: 256,
        texture_code: 4,
        raster_word: IR3_RASTER_WORD,
        hosts: &[Host::Onyx2Rack, Host::Onyx300, Host::Onyx3000],
    },
];

// The boards the GE16 products share, as the Onyx2 rack capture reads them.
const RM9: BoardSpec = BoardSpec { name: "RM9", part: "030-1402-001", revision: "E" };
const GE16_BOARD_WORD: u32 = 0x80f6_1806;
/// A raster slot word: type 6 (a TM7, the 2 and 2E) with texture code 2, and
/// type 0xb (a TM8) with code 4 (the 3).
const IR2_RASTER_WORD: u32 = 0x0302_0c06;
const IR3_RASTER_WORD: u32 = 0x0304_1606;

impl Product {
    pub fn spec(self) -> &'static ProductSpec {
        &PRODUCTS[self as usize]
    }

    /// The product `name` selects, or an error naming those there are.
    pub fn from_name(name: &str) -> Result<Product, String> {
        PRODUCTS.iter().find(|p| p.name == name).map(|p| p.product).ok_or_else(|| {
            let names: Vec<_> = PRODUCTS.iter().map(|p| p.name).collect();
            format!("kona: product {name:?}: the model has {}", names.join(", "))
        })
    }
}

/// The configuration a pipe presents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KonaConfig {
    /// The product the pipe is, a row of [`PRODUCTS`].
    pub product: Product,
    /// Raster managers: the deskside holds one or two.
    pub rm_count: u32,
    /// Which pipe slot the machine puts this on, 0 to 3: each slot's
    /// records take sixteen synthetic serial instances.
    pub identity_index: u8,
    /// The widget the pipe's DMA goes to out of reset: the host node's
    /// crossbar widget, 0 to 15. The register's real power-on value is
    /// unknown. The model starts it here so DMA works before software has
    /// programmed it; the ported model does the same with its board's hub widget.
    pub host_widget: u8,
    /// The crossbar widget the pipe sits at, which its interrupt packets name
    /// as their source. A host that decodes the vector alone, as the Onyx2's
    /// Hub does, ignores it; one that checks the sender, as Bedrock does,
    /// needs it.
    pub widget: u8,
    /// The value the probe register reads. Its bits 5:3 size the map RAM: `N << 15`
    /// entries, zero meaning 8.
    pub probe: u32,
    /// The managed area in pixels, until a video format combination sets another.
    pub managed: (u32, u32),
    /// The tile workers and the tile size in pixels, `[graphics] threads` and
    /// `tile`. Zero workers runs every job inline.
    pub threads: usize,
    pub tile: u32,
    /// The geometry board; None is the product's first.
    pub geometry: Option<Geometry>,
    /// The display generator; None is the DG5-8, or the DG5-2 behind a host board.
    pub display: Option<DisplayBoard>,
    pub pixel_depth: PixelDepth,
    pub host_board: HostBoard,
    /// The pipe slot whose records the Ktown's serial follows, when this pipe shares a Ktown
    /// with another's slot; None is its own slot.
    pub shared_identity: Option<u8>,
    /// The ROM serial of the geometry board's part, 48 bits; None is the synthetic one.
    pub geometry_serial: Option<u64>,
}

impl KonaConfig {
    /// A `product` pipe with `rm_count` raster managers in pipe slot 0.
    pub fn new(product: Product, rm_count: u32) -> Result<Self, String> {
        if rm_count != 1 && rm_count != 2 {
            return Err(format!(
                "kona: rm-count {rm_count}: an Onyx2 deskside pipe holds one or two raster managers"
            ));
        }
        Ok(KonaConfig { product, rm_count, ..KonaConfig::default() })
    }

    /// An InfiniteReality2 with `rm_count` RM7 boards in pipe slot 0.
    pub fn ir2(rm_count: u32) -> Result<Self, String> {
        Self::new(Product::Ir2, rm_count)
    }

    /// The product's row.
    pub fn spec(&self) -> &'static ProductSpec {
        self.product.spec()
    }

    /// The board the host link ends in: set before `with_rm_count`, which it allows four of.
    pub fn with_host_board(mut self, board: HostBoard) -> Self {
        self.host_board = board;
        self
    }

    /// The host board the one-wire parts a machine fits and its Ktown2 flag name: the Ktown's
    /// part number fits the rack's record, and a Ktown2 alone shows the model none.
    pub fn host_board_of(parts: &[&str], ktown2: bool) -> Result<HostBoard, String> {
        let mut board = if ktown2 { HostBoard::Ktown2 } else { HostBoard::Deskside };
        for &part in parts {
            if part != KTOWN_PART {
                return Err(format!("kona: one-wire part {part:?}: the chain can hold the Ktown, {KTOWN_PART}"));
            }
            board = HostBoard::Ktown;
        }
        Ok(board)
    }

    /// The pipe slot, 0 to 3, whose Ktown serial this pipe's Ktown record follows.
    pub fn with_shared_identity(mut self, slot: u8) -> Result<Self, String> {
        if slot >= 4 {
            return Err(format!("kona: shared identity index {slot}: each pipe slot's records use sixteen serial instances, 0 to 3"));
        }
        self.shared_identity = Some(slot);
        Ok(self)
    }

    /// The ROM serial of the geometry board's part (48 bits), in place of the synthetic one.
    pub fn with_geometry_serial(mut self, serial: u64) -> Result<Self, String> {
        if serial >> 48 != 0 {
            return Err(format!("kona: geometry serial {serial:#x} is wider than the 48 bits of a one-wire serial"));
        }
        self.geometry_serial = Some(serial);
        Ok(self)
    }

    /// Raster managers: 1, 2 or, behind a host board, 4. The pixel depth still has to fit.
    pub fn with_rm_count(mut self, rm_count: u32) -> Result<Self, String> {
        match rm_count {
            1 | 2 => {}
            4 if self.host_board == HostBoard::Deskside => {
                return Err("kona: rm-count 4: a pipe with no Ktown is an Onyx2 deskside pipe, which holds at most two raster managers".into())
            }
            4 => {}
            n => return Err(format!("kona: rm-count {n}: a pipe has 1, 2 or 4 raster managers")),
        }
        let before = self.rm_count;
        self.rm_count = rm_count;
        self.depth_fits().inspect_err(|_| self.rm_count = before)?;
        Ok(self)
    }

    /// The geometry board `name` (`ge14-4`, `ge14-2`, `ge16-4`; empty for the product's own).
    pub fn with_geometry(mut self, name: &str) -> Result<Self, String> {
        if name.is_empty() {
            self.geometry = None;
            return Ok(self);
        }
        let found = [Geometry::Ge14_4, Geometry::Ge14_2, Geometry::Ge16_4].into_iter().find(|g| g.spec().id == name);
        match found.filter(|g| self.spec().geometries.iter().any(|s| std::ptr::eq(*s, g.spec()))) {
            Some(g) => self.geometry = Some(g),
            None => {
                let takes: Vec<_> = self.spec().geometries.iter().map(|g| g.id).collect();
                return Err(format!("kona: product {} has no geometry board {name:?}; it takes {}", self.spec().name, takes.join(" or ")));
            }
        }
        Ok(self)
    }

    /// The raster manager type, which the product fixes: `rm7` or `rm9`, or empty.
    pub fn with_rm_type(self, name: &str) -> Result<Self, String> {
        let own = self.spec().raster.name.to_ascii_lowercase();
        if !name.is_empty() && name != own {
            return Err(format!("kona: product {} has {own} raster managers, not {name:?}", self.spec().name));
        }
        Ok(self)
    }

    /// Texture memory per raster manager in MB, which the product fixes: 0 for its own.
    pub fn with_texture_mb(self, mb: u32) -> Result<Self, String> {
        if mb != 0 && mb != self.spec().texture_mb {
            return Err(format!(
                "kona: product {} with {mb} MB of texture per raster manager has no known texture board; it takes {} MB",
                self.spec().name,
                self.spec().texture_mb
            ));
        }
        Ok(self)
    }

    /// The display generator `name` (`dg5-8`, `dg5-2`; empty for the default) and its channel
    /// count, 0 or the board's own. Set after the host board, which picks the default.
    pub fn with_display(mut self, name: &str, channels: u32) -> Result<Self, String> {
        self.display = match name {
            "" => None,
            "dg5-8" => Some(DisplayBoard::Dg5_8),
            "dg5-2" => Some(DisplayBoard::Dg5_2),
            "dg5-2-gvo" => return Err("kona: display dg5-2-gvo: the GVO option's channel, mask and logic download are not modelled; use dg5-2".into()),
            other => return Err(format!("kona: display {other:?} is not a display board the model presents: dg5-8 or dg5-2")),
        };
        let board = self.display_board();
        if channels != 0 && channels != board.channels {
            return Err(format!("kona: channels {channels}: display generator {} reports {}", board.board.part, board.channels));
        }
        Ok(self)
    }

    /// `small` or `medium`, or empty for small. Refused when the raster managers' pixel memory
    /// cannot hold it, by the X server's own rule.
    pub fn with_pixel_depth(mut self, name: &str) -> Result<Self, String> {
        self.pixel_depth = match name {
            "" | "small" => PixelDepth::Small,
            "medium" => PixelDepth::Medium,
            other => return Err(format!("kona: pixel-depth {other:?} is not modelled; the model presents small or medium")),
        };
        self.depth_fits()?;
        Ok(self)
    }

    /// The X server's rule: raster managers times 20,971,520 over the managed area's height and
    /// its width rounded up to 160 must be at least 8 pixel bytes for Small and 16 for Medium.
    fn depth_fits(&self) -> Result<(), String> {
        let area = self.managed.1 as u64 * (self.managed.0 as u64).div_ceil(160) * 160;
        if area != 0 && (self.rm_count as u64 * 20_971_520 / area) < self.pixel_depth.need() {
            return Err(format!("kona: pixel-depth {:?} needs more pixel memory than {} raster managers hold at {}x{}", self.pixel_depth, self.rm_count, self.managed.0, self.managed.1));
        }
        Ok(())
    }

    /// The geometry board the pipe has.
    pub(super) fn geometry_spec(&self) -> &'static GeometrySpec {
        self.geometry.map_or(self.spec().geometries[0], |g| g.spec())
    }

    /// The display generator the pipe has.
    pub(super) fn display_board(&self) -> &'static Display {
        self.display.unwrap_or(if self.host_board == HostBoard::Deskside { DisplayBoard::Dg5_8 } else { DisplayBoard::Dg5_2 }).spec()
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

    /// The crossbar widget the pipe sits at, 0 to 15.
    pub fn with_widget(mut self, widget: u8) -> Result<Self, String> {
        if widget > 15 {
            return Err(format!("kona: widget {widget}: a widget number is four bits"));
        }
        self.widget = widget;
        Ok(self)
    }

    /// The widget control register's value out of reset.
    pub(super) fn widget_control_reset(&self) -> u32 {
        (self.host_widget as u32) << 22
    }

    /// The machine's `[graphics]` worker count and tile size.
    pub fn with_pool(mut self, threads: usize, tile: u32) -> Self {
        self.threads = threads;
        self.tile = tile;
        self
    }

    /// The most words a pixel's record may take at the managed size.
    pub fn record_words(&self) -> u32 {
        super::back::store::layout::record_words(self.rm_count, self.managed.0, self.managed.1)
    }

    /// Texture memory on each raster manager's texture board, in bytes.
    pub fn texture_bytes_per_rm(&self) -> u64 {
        (self.spec().texture_mb as u64) << 20
    }

    fn base(&self) -> u32 {
        16 * self.identity_index as u32
    }
}

impl Default for KonaConfig {
    fn default() -> Self {
        KonaConfig { product: Product::Ir2, rm_count: 2, identity_index: 0, host_widget: 0, widget: 0, probe: 0, managed: (1280, 1024), threads: 2, tile: 32, geometry: None, display: None, pixel_depth: PixelDepth::Small, host_board: HostBoard::Deskside, shared_identity: None, geometry_serial: None }
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

/// The Ktown board's part number, which a machine names to fit its record on the chain.
pub(super) const KTOWN_PART: &str = "030-1067-001";

/// The marker of a synthetic serial and of the UniqueWare form a part's own serial may take.
const SERIAL_PREFIX: u64 = 0x5145_4d55;
const UNIQUEWARE_MARKER: u64 = 0x45_4d55;

/// The barcode for the record on a part whose ROM serial is `serial`: the letter, then the
/// part's instance, so the record and the part name one unit. An unmarked serial, such as an
/// override, gives instance 0.
fn barcode_of(letter: char, serial: u64) -> String {
    let n = if serial >> 16 == SERIAL_PREFIX {
        (serial & 0xff) as u32
    } else if (serial >> 12) & 0xff_ffff == UNIQUEWARE_MARKER {
        (serial & 0xfff) as u32
    } else {
        0
    };
    barcode(letter, n)
}

/// The identity bus: the geometry board's record on a DS2502, and a rack's Ktown record.
pub(super) struct Identity {
    pub bus: Bus,
}

impl Identity {
    pub fn new(cfg: &KonaConfig) -> Self {
        let geometry = &cfg.geometry_spec().board;
        let serial = cfg.geometry_serial.unwrap_or(synthetic_serial(b'G', cfg.base()));
        let record = BoardRecord { barcode: &barcode_of('G', serial), part: geometry.part, revision: geometry.revision, name: geometry.name };
        let mut bus = Bus::new();
        bus.add(Box::new(Ds2502::new(ONEWIRE_FAMILY, serial, &record.image())));
        if cfg.host_board == HostBoard::Ktown {
            // The Ktown's serial follows its slot, the shared one when two pipes share it.
            let slot = cfg.shared_identity.unwrap_or(cfg.identity_index) as u32;
            let serial = synthetic_serial(b'G', 16 * slot + 1);
            let record = BoardRecord { barcode: &barcode_of('G', serial), part: KTOWN_PART, revision: "F", name: "KTOWN" };
            bus.add(Box::new(Ds2502::new(ONEWIRE_FAMILY, serial, &record.image())));
        }
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
/// product's row (0 for an InfiniteReality2), after `SELECT_REDUCED` bit 10
/// would make it a Reality (0).
pub(super) fn class_word(cfg: &KonaConfig, selector: u32) -> Option<u32> {
    match selector {
        SELECT_PRODUCT => Some(cfg.spec().class_word),
        SELECT_REDUCED => Some(0),
        _ => None,
    }
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
    [
        Pool { max_key: 0, max_align: 0x4000, copies: 1, shared: 0, copy_down: 1, granule: 0x400, capacity: 0x100 },
        Pool { max_key: 0, max_align: 0x4000, copies: 1, shared: 1, copy_down: 1, granule: 0x400, capacity: 0x3c00 },
        Pool { max_key: 10, max_align: 0x4000, copies: cfg.geometry_spec().engines, shared: 0, copy_down: 1, granule: 0x400, capacity: 0x400 },
        Pool { max_key: 0, max_align: 0x4000, copies: 1, shared: 0, copy_down: 1, granule: 0x400, capacity: cfg.spec().texture_mb * (1 << 20) / 0x400 },
    ]
}

/// The configuration block the pipe answers a configuration request with:
/// every field at the offset the driver's board report reads it from.
/// `width` and `height` are the managed area's, 1280x1024 until a video
/// format combination sets up another.
pub(super) fn config_block(cfg: &KonaConfig, width: u32, height: u32) -> Vec<u8> {
    let mut b = vec![0u8; CONFIG_SIZE];
    let base = cfg.base();
    let spec = cfg.spec();

    let (geometry, display) = (cfg.geometry_spec(), cfg.display_board());

    put(&mut b, CFG_BOARD_SET, spec.board_set);
    put(&mut b, CFG_ENGINES, geometry.engines);
    put(&mut b, CFG_RMS, cfg.rm_count);
    put(&mut b, CFG_FIFO_RAM, FIFO_RAM_4MB_EXTERNAL);
    put(&mut b, CFG_CHANNELS, display.channels);
    put(&mut b, CFG_VIDEO_OPTION, 0);
    put(&mut b, CFG_GAMMA, display.external_gamma as u32);
    // The display generator's own channel count, which the combination
    // checker compares a combination's enabled channels against.
    put(&mut b, CFG_DG_CHANNEL_COUNT, display.channels);
    put(&mut b, CFG_ACTIVE_ENGINES, geometry.engines);
    put(&mut b, CFG_ACTIVE_RMS, cfg.rm_count);
    put(&mut b, CFG_OCCUPANCY, geometry.occupancy);
    put(&mut b, CFG_WIDTH, width);
    put(&mut b, CFG_HEIGHT, height);
    put(&mut b, CFG_PIXEL_DEPTH, cfg.pixel_depth as u32);

    for (i, p) in pools(cfg).iter().enumerate() {
        let at = CFG_POOLS + 0x1c * i;
        for (k, w) in [p.max_key, p.max_align, p.copies, p.shared, p.copy_down, p.granule, p.capacity]
            .into_iter()
            .enumerate()
        {
            put(&mut b, at + 4 * k, w);
        }
    }

    put(&mut b, CFG_BOARD_WORD, geometry.board_word);
    for (i, w) in [0, 0x1483_2057, 0x2473_1057, 0x1423_1057].into_iter().enumerate() {
        put(&mut b, CFG_GE_WORDS + 4 * i, w);
    }
    for i in 0..cfg.rm_count as usize {
        let chips = CFG_RM_CHIPS + 0x8c * i;
        put(&mut b, CFG_RM_PRESENT + 8 * i, 1);
        put(&mut b, CFG_RM_TEXTURE + 8 * i, spec.texture_code);
        put(&mut b, chips, spec.raster_word);
        put(&mut b, chips + 0x04, 0x1503_2057);
        put(&mut b, chips + 0x08, 0x1543_1057);
        put_words(&mut b, chips + 0x0c, 4, 0x2463_1057);
        put_words(&mut b, chips + 0x1c, 8, 0x24d3_1057);
        put_words(&mut b, chips + 0x3c, 20, 0x5493_8057);
        put_record(&mut b[CFG_NIC_RM + 0x4c * i..][..0x4c], &spec.raster, base + 2 + i as u32);
        put_record(&mut b[CFG_NIC_TM + 0x4c * i..][..0x4c], &spec.texture, base + 6 + i as u32);
    }
    put(&mut b, CFG_TRAILING_WORD, display.word());
    put(&mut b, CFG_DG_WORDS, 0x0553_2057);
    put_words(&mut b, CFG_DG_WORDS + 4, 4, 0x1583_8057);
    put_words(&mut b, CFG_DG_CHANNELS, display.channels as usize, 0x1563_1057);
    put_record(&mut b[CFG_NIC_DG..][..0x4c], &display.board, base + 10);
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
        assert!(KonaConfig::ir2(4).is_err(), "four need a host board");
        assert!(KonaConfig::ir2(0).is_err());
    }

    fn word(block: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(block[at..at + 4].try_into().unwrap())
    }

    /// Four raster managers need a host board, and then fill the block's four slots exactly: the
    /// last chip words end where the trailing word starts.
    #[test]
    fn four_raster_managers_need_a_host_board_and_fill_the_block() {
        assert!(KonaConfig::ir2(1).unwrap().with_rm_count(4).is_err(), "a deskside holds two");
        for board in [HostBoard::Ktown, HostBoard::Ktown2] {
            let cfg = KonaConfig::new(Product::Ir3, 1).unwrap().with_host_board(board).with_rm_count(4).unwrap();
            let block = config_block(&cfg, 1280, 1024);
            assert_eq!((word(&block, CFG_RMS), word(&block, CFG_ACTIVE_RMS)), (4, 4));
            for i in 0..4 {
                assert_eq!((word(&block, CFG_RM_PRESENT + 8 * i), word(&block, CFG_RM_TEXTURE + 8 * i), word(&block, CFG_RM_CHIPS + 0x8c * i)), (1, 4, IR3_RASTER_WORD), "raster manager {i}");
            }
            assert_eq!(word(&block, CFG_TRAILING_WORD), DG5_2.word(), "the DG5-2 is the default behind a host board");
        }
        assert!(KonaConfig::ir2(2).unwrap().with_rm_count(3).is_err());
    }

    /// The geometry board sets the engine count, occupancy, pool copies and board word, and its
    /// own record is the chain's.
    #[test]
    fn the_geometry_board_is_an_input_the_product_limits() {
        let cfg = KonaConfig::ir2(1).unwrap().with_geometry("ge14-2").unwrap();
        let block = config_block(&cfg, 1280, 1024);
        assert_eq!((word(&block, CFG_ENGINES), word(&block, CFG_ACTIVE_ENGINES), word(&block, CFG_OCCUPANCY)), (2, 2, 3));
        assert_eq!(word(&block, CFG_POOLS + 0x1c * 2 + 8), 2, "one copy of the geometry class per engine");
        assert_eq!(cfg.geometry_spec().board.part, "030-1051-002");
        assert!(KonaConfig::ir2(1).unwrap().with_geometry("ge16-4").is_err(), "the InfiniteReality2 takes GE14 boards");
        assert!(KonaConfig::new(Product::Ir2e, 1).unwrap().with_geometry("ge14-4").is_err());
        assert_eq!(KonaConfig::new(Product::Ir3, 1).unwrap().with_geometry("ge16-4").unwrap().geometry_spec().board.name, "GE16-4");
    }

    #[test]
    fn the_display_board_sets_its_channels_gamma_and_identity() {
        let cfg = KonaConfig::ir2(1).unwrap().with_display("dg5-2", 2).unwrap();
        let block = config_block(&cfg, 1280, 1024);
        assert_eq!((word(&block, CFG_CHANNELS), word(&block, CFG_DG_CHANNEL_COUNT), word(&block, CFG_GAMMA)), (2, 2, 0));
        assert_eq!(word(&block, CFG_TRAILING_WORD), DG5_2.word());
        assert_eq!(word(&block, CFG_DG_CHANNELS + 4 * 2), 0, "no word for a third channel");
        assert!(KonaConfig::ir2(1).unwrap().with_display("dg5-2", 8).is_err());
        assert!(KonaConfig::ir2(1).unwrap().with_display("dg5-8", 2).is_err());
        assert!(KonaConfig::ir2(1).unwrap().with_display("dg5-2-gvo", 0).unwrap_err().contains("not modelled"));
        assert!(KonaConfig::ir2(1).unwrap().with_display("dg9", 0).is_err());
    }

    /// The raster manager type and the texture board are the product's, as the old device had them.
    #[test]
    fn the_raster_type_and_texture_board_are_the_products() {
        assert!(KonaConfig::ir2(1).unwrap().with_rm_type("rm7").is_ok());
        assert!(KonaConfig::ir2(1).unwrap().with_rm_type("rm9").is_err());
        assert!(KonaConfig::new(Product::Ir3, 1).unwrap().with_rm_type("rm9").is_ok());
        assert!(KonaConfig::ir2(1).unwrap().with_texture_mb(64).is_ok());
        assert!(KonaConfig::ir2(1).unwrap().with_texture_mb(256).is_err());
        assert!(KonaConfig::new(Product::Ir3, 1).unwrap().with_texture_mb(256).is_ok());
    }

    /// Medium pixels need sixteen pixel bytes of the raster managers' memory at 1280x1024, which
    /// one manager has; a bigger managed area needs more managers.
    #[test]
    fn the_pixel_depth_is_reported_and_must_fit_the_raster_managers() {
        let cfg = KonaConfig::ir2(1).unwrap().with_pixel_depth("medium").unwrap();
        assert_eq!(word(&config_block(&cfg, 1280, 1024), CFG_PIXEL_DEPTH), 1);
        assert_eq!(word(&config_block(&KonaConfig::ir2(1).unwrap(), 1280, 1024), CFG_PIXEL_DEPTH), 0);
        let mut big = KonaConfig::ir2(1).unwrap();
        big.managed = (1920, 1200);
        assert!(big.with_pixel_depth("medium").is_err(), "one manager cannot hold medium pixels at 1920x1200");
        assert!(KonaConfig::ir2(1).unwrap().with_pixel_depth("large").is_err());
    }

    #[test]
    fn the_host_widget_sets_the_widget_control_reset_value() {
        assert_eq!(KonaConfig::default().widget_control_reset(), 0);
        let cfg = KonaConfig::default().with_host_widget(9).unwrap();
        assert_eq!(cfg.widget_control_reset(), 9 << 22);
        assert!(KonaConfig::default().with_host_widget(16).is_err());
    }

    pub(in crate::dev::kona) fn synthetic_serial(instance: u32) -> u64 {
        0x5145_4d55u64 << 16 | (b'G' as u64) << 8 | instance as u64
    }

    /// A block as `config-ir2-*rm.hex` holds it: sixteen bytes a row, in hex.
    pub(in crate::dev::kona) fn parse_hex(text: &str) -> Vec<u8> {
        text.split_whitespace().map(|b| u8::from_str_radix(b, 16).unwrap()).collect()
    }

    /// The block the pipe builds is the deskside's byte for byte, for two
    /// raster managers and for one. The files hold the fixed bytes.
    #[test]
    fn the_configuration_block_matches_the_deskside_layout() {
        for (rms, want) in [(2, include_str!("tests/data/config-ir2-2rm.hex")), (1, include_str!("tests/data/config-ir2-1rm.hex"))] {
            let cfg = KonaConfig::ir2(rms).unwrap();
            let block = config_block(&cfg, 1280, 1024);
            let want = parse_hex(want);
            assert_eq!(block.len(), want.len());
            if let Some(i) = (0..block.len()).step_by(4).find(|&i| block[i..i + 4] != want[i..i + 4]) {
                panic!("{rms} RMs: first difference at {i:#x}: {:x?} against {:x?}", &block[i..i + 4], &want[i..i + 4]);
            }
        }
    }

    /// `Product::spec` indexes the table by the enum, so a row out of order
    /// would present another product's boards.
    #[test]
    fn each_product_indexes_its_own_row() {
        for (i, row) in PRODUCTS.iter().enumerate() {
            assert_eq!(row.product as usize, i, "{}", row.name);
            assert_eq!(row.product.spec().name, row.name);
            assert_eq!(Product::from_name(row.name), Ok(row.product));
        }
        let e = Product::from_name("ir9").unwrap_err();
        assert!(e.contains("ir2"), "{e}");
    }

    /// The display word is built from the board: the DG5-8's and the DG5-2 with
    /// a GVO option's match the words the captures read.
    #[test]
    fn the_display_word_follows_the_display_board() {
        assert_eq!(DG5_8.word(), 0x51bf_1002);
        let dg5_2_gvo = Display { channels: 2, option: 6, colour_map: false, external_gamma: false, high: 0xf9, board: BoardSpec { name: "", part: "030-1242-001", revision: "D" } };
        assert_eq!(dg5_2_gvo.word(), 0xf931_1002);
        assert_eq!(Display { option: 7, ..dg5_2_gvo }.word(), 0xf939_1002);
    }

    #[test]
    fn the_managed_size_is_reported() {
        let block = config_block(&KonaConfig::default(), 640, 480);
        assert_eq!(u32::from_be_bytes(block[0x84..0x88].try_into().unwrap()), 640);
        assert_eq!(u32::from_be_bytes(block[0x88..0x8c].try_into().unwrap()), 480);
    }
}
