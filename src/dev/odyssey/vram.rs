//! The board's memory as the board addresses it: one array of 64-bit words,
//! of the size the strap reports, and surfaces over it named the way the
//! command stream names them (block 0x017, the texture level descriptors,
//! the copy operands).
//!
//! Memory is words, as main memory is. A field narrower than a word, n bytes
//! at byte address a, is the bit range of its word that a 64-bit load of
//! guest memory gives those addresses: bits (8 - a % 8 - n) * 8 up, the
//! lowest address highest. Nothing converts byte order: DMA moves the
//! guest's words in and out whole, and every field is read and written by
//! shift and mask (`VramPtr::load`, `store`).
//!
//! A surface is a base address in 16-byte units, a pitch in 16 x 16-pixel
//! tiles and a pixel size of 2 << n bytes (n, the frame-buffer depth, from
//! bits 15:13 of the pixel mode's status word). Tiles are stored in row-major order of tiles, pixels in
//! row-major order inside a tile. The order inside a tile is the model's
//! choice: nothing the host can read shows the board's, since data reaches
//! memory only through the board's DMA and copies. Measurements and the
//! format word's decode: `ignore/odsy/docs/measured.md` sections 4 and 5,
//! `clanker/plans/odyssey-board-memory.md`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// Bytes in one unit of a base address.
pub(super) const UNIT: u64 = 16;
/// Pixels on a side of a tile.
pub(super) const TILE: u32 = 16;
/// The address bits of a base word; the top three are flags.
pub(super) const ADDR_MASK: u32 = 0x1fff_ffff;
/// Bit 28 of a copy's address word: the operand is the current drawable
/// (0x10000019 its draw buffer, 0x10000010 its read buffer), not memory.
pub(super) const DRAWABLE: u32 = 0x1000_0000;
/// Word 1 of a select: the pitch in tiles.
pub(super) const PITCH_MASK: u32 = 0xfff;

/// Fields of the pixel mode's status word (block 0x017 word 2). The two
/// depths are bytes a pixel, 2 << code.
pub(super) mod fmt {
    pub const FB_DEPTH_SHIFT: u32 = 13; // the memory's pixel
    pub const WIN_DEPTH_SHIFT: u32 = 10; // the part of it the visual uses
    pub const PIXEL_FMT_SHIFT: u32 = 7; // how the visual arranges that part
    pub const FIELD_FMT_SHIFT: u32 = 4; // the colour field's components
    pub const MASK3: u32 = 7;
    pub const PIXEL_INDEX: u32 = 1 << 19; // colour-index visual
    pub const SCRATCH: u32 = 1 << 27; // no buffer select, window clip or offset
}

/// A status word, split into its fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Format {
    /// Bytes a memory pixel is 1 << fb_log2.
    pub fb_log2: u8,
    /// Bytes of it the visual uses, 1 << win_log2.
    pub win_log2: u8,
    pub pixel_fmt: u8,
    pub field_fmt: u8,
    pub index: bool,
    pub scratch: bool,
}

impl Format {
    pub fn decode(w: u32) -> Format {
        let f = |s: u32| ((w >> s) & fmt::MASK3) as u8;
        Format {
            fb_log2: f(fmt::FB_DEPTH_SHIFT) + 1,
            win_log2: f(fmt::WIN_DEPTH_SHIFT) + 1,
            pixel_fmt: f(fmt::PIXEL_FMT_SHIFT),
            field_fmt: f(fmt::FIELD_FMT_SHIFT),
            index: w & fmt::PIXEL_INDEX != 0,
            scratch: w & fmt::SCRATCH != 0,
        }
    }
}

/// A rectangle of board memory as a surface: where it starts, its row of
/// tiles and its pixel size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Surface {
    /// In 16-byte units.
    pub base: u32,
    /// Tiles in a row of tiles.
    pub pitch: u32,
    /// Bytes a pixel is 1 << bpp_log2.
    pub bpp_log2: u8,
}

#[cfg(test)]
impl Surface {
    /// Bytes from the base to the end of row of tiles rows-1.
    pub fn bytes(&self, rows: u32) -> u64 {
        (rows.div_ceil(TILE) as u64 * self.pitch as u64 * (TILE * TILE) as u64) << self.bpp_log2
    }

    pub fn width(&self) -> u32 {
        self.pitch * TILE
    }
}

impl Surface {
    pub fn new(base: u32, pitch: u32, bpp_log2: u8) -> Surface {
        Surface { base: base & ADDR_MASK, pitch, bpp_log2 }
    }

#[cfg(test)]
    /// The surface a pixel mode (block 0x017) selects: words 0 (origin), 1
    /// (stride) and 2 (status).
    pub fn from_select(w: &[u32]) -> Surface {
        Surface::new(w[0], w[1] & PITCH_MASK, Format::decode(w[2]).fb_log2)
    }

    pub fn bytes_per_pixel(&self) -> u32 {
        1 << self.bpp_log2
    }

    /// The byte address of pixel (x, y).
    pub fn offset(&self, x: u32, y: u32) -> u64 {
        let tile = (y / TILE) as u64 * self.pitch as u64 + (x / TILE) as u64;
        let pixel = tile * (TILE * TILE) as u64 + ((y % TILE) * TILE + x % TILE) as u64;
        self.base as u64 * UNIT + (pixel << self.bpp_log2)
    }

}

/// Bytes in one block of the dirty map, log2.
pub(super) const DIRTY_BLOCK_SHIFT: u32 = 14;

/// Which blocks changed since a map was last taken: `bits` is what the CPU
/// stored, for the GPU unit's mirror to upload; `reported` is every change by
/// any unit, for a shell that lent the memory. Each is kept only when asked.
#[cfg_attr(not(feature = "wgpu"), allow(dead_code, reason = "used by the GPU raster unit"))]
pub(super) struct DirtyBlocks {
    bits: Box<[AtomicU64]>,
    reported: Box<[AtomicU64]>,
    /// What the last `take_reported` took, which the next one reports again.
    again: Box<[AtomicU64]>,
    blocks: u64,
}

#[cfg_attr(not(feature = "wgpu"), allow(dead_code, reason = "used by the GPU raster unit"))]
impl DirtyBlocks {
    fn new(size: u64) -> DirtyBlocks {
        let blocks = (size >> DIRTY_BLOCK_SHIFT).max(1);
        let map = || (0..blocks.div_ceil(64)).map(|_| AtomicU64::new(0)).collect::<Box<[_]>>();
        DirtyBlocks { bits: map(), reported: map(), again: map(), blocks }
    }

    /// Mark the block holding word index w for upload.
    #[inline(always)]
    fn mark_word(&self, w: usize) {
        let b = w >> (DIRTY_BLOCK_SHIFT - 3);
        self.bits[b >> 6].fetch_or(1 << (b & 63), Ordering::Relaxed);
    }

    /// Mark the blocks under `len` bytes at byte offset `off`, in the upload
    /// map or the shell's.
    fn mark_range(&self, off: u64, len: u64, upload: bool) {
        if len == 0 || off >= self.blocks << DIRTY_BLOCK_SHIFT {
            return;
        }
        let map = if upload { &self.bits } else { &self.reported };
        let last = ((off + len - 1) >> DIRTY_BLOCK_SHIFT).min(self.blocks - 1);
        for b in (off >> DIRTY_BLOCK_SHIFT)..=last {
            map[(b >> 6) as usize].fetch_or(1 << (b & 63), Ordering::Relaxed);
        }
    }

    pub fn clear(&self) {
        for w in self.bits.iter() {
            w.store(0, Ordering::Relaxed);
        }
    }

    /// The upload map's marked blocks as runs of bytes (offset, length), unmarked.
    pub fn take_runs(&self) -> Vec<(u64, u64)> {
        self.take(&self.bits, None)
    }

    /// The shell's map as runs of bytes, unmarked, plus what the last call took
    /// while threads ran (`running`): a store racing that take may not re-mark.
    pub fn take_reported(&self, running: bool) -> Vec<(u64, u64)> {
        self.take(&self.reported, Some((&self.again[..], running)))
    }

    fn take(&self, map: &[AtomicU64], again: Option<(&[AtomicU64], bool)>) -> Vec<(u64, u64)> {
        let mut runs: Vec<(u64, u64)> = Vec::new();
        let block = 1u64 << DIRTY_BLOCK_SHIFT;
        for (i, w) in map.iter().enumerate() {
            // Most words are clear: read before the swap writes.
            if w.load(Ordering::Relaxed) == 0 && again.is_none_or(|(a, _)| a[i].load(Ordering::Relaxed) == 0) {
                continue;
            }
            let taken = w.swap(0, Ordering::Relaxed);
            let mut bits = taken | again.map_or(0, |(a, keep)| a[i].swap(if keep { taken } else { 0 }, Ordering::Relaxed));
            while bits != 0 {
                let b = (i as u64) * 64 + bits.trailing_zeros() as u64;
                bits &= bits - 1;
                if b >= self.blocks {
                    break;
                }
                let off = b * block;
                match runs.last_mut() {
                    Some((o, l)) if *o + *l == off => *l += block,
                    _ => runs.push((off, block)),
                }
            }
        }
        runs
    }
}

/// The memory's words: its own, or a region the shell lent.
enum Words {
    Own(Vec<u64>),
    Lent { ptr: *mut u64, len: usize },
}

// SAFETY: a lent region outlives the board by the lender's contract
// (`Vram::lent`) and is reached as `VramPtr` reaches the array.
unsafe impl Send for Words {}
unsafe impl Sync for Words {}

impl Default for Words {
    fn default() -> Self {
        Words::Own(Vec::new())
    }
}

impl Clone for Words {
    /// A copy is the clone's own, never a second view of the lent region.
    fn clone(&self) -> Self {
        Words::Own(self.to_vec())
    }
}

impl std::ops::Deref for Words {
    type Target = [u64];
    fn deref(&self) -> &[u64] {
        match self {
            Words::Own(v) => v,
            // SAFETY: `Vram::lent`'s contract.
            Words::Lent { ptr, len } => unsafe { std::slice::from_raw_parts(*ptr, *len) },
        }
    }
}

impl std::ops::DerefMut for Words {
    fn deref_mut(&mut self) -> &mut [u64] {
        match self {
            Words::Own(v) => v,
            // SAFETY: as above, and the board is the region's only writer.
            Words::Lent { ptr, len } => unsafe { std::slice::from_raw_parts_mut(*ptr, *len) },
        }
    }
}

/// The board's memory.
#[derive(Clone, Default)]
pub(super) struct Vram {
    words: Words,
    /// The dirty maps, once either is kept; `upload` says the GPU unit's
    /// mirror needs the one, `report` that the shell lent the memory.
    dirty: Option<Arc<DirtyBlocks>>,
    upload: bool,
    report: bool,
    /// Set by every store: the display composes again when it finds it set
    /// (`VramPtr::take_written`).
    written: Arc<AtomicBool>,
}

impl Vram {
    /// Memory of at least size bytes, a power of two, so an address masks
    /// into it.
    pub fn new(size: u64) -> Vram {
        Vram { words: Words::Own(vec![0; (size.next_power_of_two() / 8).max(1) as usize]), dirty: None, upload: false, report: false, written: Arc::default() }
    }

    /// Memory of `bytes` at `ptr`, which the caller owns: the shell lends
    /// board memory it can map, save and watch.
    ///
    /// # Safety
    /// `ptr` is 8-byte aligned and valid for reads and writes of `bytes`
    /// until the board is dropped, and nothing else writes it meanwhile.
    pub unsafe fn lent(ptr: *mut u64, bytes: u64) -> Vram {
        assert!(bytes.is_power_of_two() && bytes >= 8, "board memory is a power of two");
        Vram { words: Words::Lent { ptr, len: (bytes / 8) as usize }, dirty: None, upload: false, report: false, written: Arc::default() }
    }

    /// Start keeping the dirty map (the GPU instantiation's mirror).
    #[cfg_attr(not(feature = "wgpu"), allow(dead_code, reason = "used by the GPU raster unit"))]
    pub fn track_dirty(&mut self) {
        self.dirty_map();
        self.upload = true;
    }

    /// Keep the shell's map of what changed, for memory the shell lent.
    /// Before the board's threads start.
    pub fn report_dirty(&mut self) {
        self.dirty_map();
        self.report = true;
    }

    fn dirty_map(&mut self) {
        let size = self.size();
        self.dirty.get_or_insert_with(|| Arc::new(DirtyBlocks::new(size)));
    }

    /// Its size in bytes.
    pub fn size(&self) -> u64 {
        self.words.len() as u64 * 8
    }

    /// The memory as the raster unit and the display reach it. The array is
    /// sized once, at allocation, and never moves, so the pointer stays
    /// good for the board's life.
    pub fn ptr(&mut self) -> VramPtr {
        VramPtr { ptr: self.words.as_mut_ptr(), mask: self.words.len() - 1, dirty: self.dirty_ptr(), maps: self.maps_ptr(), report: self.report_ptr(), written: Arc::as_ptr(&self.written) }
    }

    /// ptr() from a shared borrow: the memory is written through the pointer
    /// by units that race as the board's do (see VramPtr).
    pub fn ptr_shared(&self) -> VramPtr {
        VramPtr { ptr: self.words.as_ptr() as *mut u64, mask: self.words.len() - 1, dirty: self.dirty_ptr(), maps: self.maps_ptr(), report: self.report_ptr(), written: Arc::as_ptr(&self.written) }
    }

    /// The maps, or null when the GPU unit's upload does not need one.
    fn dirty_ptr(&self) -> *const DirtyBlocks {
        if self.upload { self.maps_ptr() } else { std::ptr::null() }
    }

    fn maps_ptr(&self) -> *const DirtyBlocks {
        self.dirty.as_ref().map_or(std::ptr::null(), |d| Arc::as_ptr(d))
    }

    /// The words of the shell's map, or null when the shell watches nothing.
    fn report_ptr(&self) -> *const AtomicU64 {
        match &self.dirty {
            Some(d) if self.report => d.reported.as_ptr(),
            _ => std::ptr::null(),
        }
    }

    /// Zero the memory, then mark it written: a frame composed while the
    /// fill ran saw it half cleared, and the mark after the last word
    /// brings the next frame.
    pub fn clear(&mut self) {
        self.words.fill(0);
        if let (Some(d), true) = (&self.dirty, self.report) {
            d.mark_range(0, self.words.len() as u64 * 8, false);
        }
        self.written.store(true, Ordering::Relaxed);
    }

    /// The memory as words, for the GPU unit's tests. It does not mark the
    /// memory written: a caller that changes the picture marks it after.
    #[cfg(all(test, feature = "wgpu"))]
    pub fn words_mut(&mut self) -> &mut [u64] {
        &mut self.words
    }
}

/// Board memory shared with the raster unit and the display thread, which
/// read and write it with no lock, as the board's own units do: the front
/// end waits for the raster unit before it touches memory itself
/// (Board::raster_wait), and a display frame that reads a pixel mid-store
/// shows it for one frame.
#[derive(Clone, Copy)]
pub(super) struct VramPtr {
    ptr: *mut u64,
    /// Words less one: a word index masks into the memory.
    mask: usize,
    /// The dirty maps, or null when the GPU unit's upload keeps none
    /// (Vram::track_dirty): what `store` marks for upload.
    dirty: *const DirtyBlocks,
    /// The maps whenever any is kept, for ranges.
    maps: *const DirtyBlocks,
    /// The words of the shell's map, or null (Vram::report_dirty).
    report: *const AtomicU64,
    /// `Vram::written`, which outlives the pointer like the array.
    written: *const AtomicBool,
}

/// Whether the n bytes at byte address a do not lie in one aligned 8-byte word. Every field the
/// library and the display modes name in the evidence is whole inside its word; a header or
/// mode that places one across two is a guest error, which the model does not guess at: it
/// reads 0 and its stores are dropped, in every build, reported once.
#[inline(always)]
pub(super) fn straddles(a: u64, n: u32) -> bool {
    let bad = (a & 7) as u32 + n > 8;
    if bad {
        straddle_reported(a, n);
    }
    bad
}

#[cold]
#[inline(never)]
fn straddle_reported(a: u64, n: u32) {
    static SEEN: AtomicBool = AtomicBool::new(false);
    if !SEEN.swap(true, Ordering::Relaxed) {
        crate::dlog_guest!(crate::devlog::LogModule::Odyssey, "sgi-odyssey: a {n}-byte field at byte {:#x} runs past its 8-byte word; it reads 0 and its stores are dropped", a);
    }
}

/// Where a null view's stores, which never happen, would mark.
static NOWHERE: AtomicBool = AtomicBool::new(false);

// SAFETY: the array outlives every user (Board owns it and stops the raster
// and display threads before it drops), and never moves.
unsafe impl Send for VramPtr {}
unsafe impl Sync for VramPtr {}

impl VramPtr {
    /// The word holding byte address a.
    #[inline(always)]
    fn word(&self, a: u64) -> *mut u64 {
        // SAFETY: masked into the array.
        unsafe { self.ptr.add((a >> 3) as usize & self.mask) }
    }

    /// The n words from the one holding byte address a, for a reader that
    /// takes a run of them at once (the display's rows); None when the run
    /// wraps the end of memory. Racing writers as `load` does.
    #[inline(always)]
    pub fn words(&self, a: u64, n: usize) -> Option<&[u64]> {
        let i = (a >> 3) as usize & self.mask;
        if i + n > self.mask + 1 {
            return None;
        }
        // SAFETY: in the array, which outlives the pointer (see the type).
        Some(unsafe { std::slice::from_raw_parts(self.ptr.add(i), n) })
    }

    /// The value of the n bytes (1 to 8) at byte address a, which lie in
    /// one word: the bits of it they are, the lowest address highest. Bytes
    /// that run past their word (a field a guest's header or display mode
    /// places across two) read as 0: see `straddles`.
    #[inline(always)]
    pub fn load(&self, a: u64, n: u32) -> u64 {
        if straddles(a, n) {
            return 0;
        }
        // SAFETY: the word is in the memory; readers and writers race as
        // the board's units do (see the type).
        let w = unsafe { *self.word(a) };
        let shift = 64 - 8 * ((a & 7) as u32 + n);
        if n == 8 { w } else { (w >> shift) & ((1u64 << (8 * n)) - 1) }
    }

    /// Store v as the n bytes at byte address a (load()'s bits), and mark
    /// the memory written for the display.
    #[inline(always)]
    pub fn store(&self, a: u64, n: u32, v: u64) {
        self.store_quiet(a, n, v);
        self.mark_written();
    }

    /// `store` without the mark, for a loop that calls `mark_written`
    /// once after its last store, so the loop stays free of an atomic.
    #[inline(always)]
    pub fn store_quiet(&self, a: u64, n: u32, v: u64) {
        if straddles(a, n) {
            return;
        }
        let p = self.word(a);
        // SAFETY: as load().
        unsafe {
            if n == 8 {
                *p = v;
            } else {
                let shift = 64 - 8 * ((a & 7) as u32 + n);
                let m = ((1u64 << (8 * n)) - 1) << shift;
                *p = (*p & !m) | ((v << shift) & m);
            }
            let w = (a >> 3) as usize & self.mask;
            if !self.dirty.is_null() {
                (*self.dirty).mark_word(w);
            }
            // Raster threads all store here, so a bit already set is only
            // read: no atomic write, and the line stays shared.
            if !self.report.is_null() {
                let b = w >> (DIRTY_BLOCK_SHIFT - 3);
                let (word, bit) = (&*self.report.add(b >> 6), 1u64 << (b & 63));
                if word.load(Ordering::Relaxed) & bit == 0 {
                    word.fetch_or(bit, Ordering::Relaxed);
                }
            }
        }
    }

    /// Mark the memory written. A relaxed store, not a read-modify-write and
    /// with no ordering against the data: a display that takes the mark
    /// before the data it marks is visible composes the old data, and the
    /// frame after composes it again (`ScanoutReader::update`).
    #[inline(always)]
    pub fn mark_written(&self) {
        // SAFETY: the flag lives as long as the memory (Vram::written).
        unsafe { (*self.written).store(true, Ordering::Relaxed) }
    }

    /// Whether anything was stored since the last call, which clears it:
    /// the display takes it before it reads the memory, so a store that
    /// lands during the frame marks the next one.
    pub fn take_written(&self) -> bool {
        // SAFETY: the flag lives as long as the memory (Vram::written).
        unsafe { (*self.written).swap(false, Ordering::Relaxed) }
    }

    /// The same memory with no dirty map: for a writer that uploads what
    /// it wrote itself (`GpuRaster::upload_region`).
    pub fn untracked(&self) -> VramPtr {
        VramPtr { dirty: std::ptr::null(), ..*self }
    }

    /// The dirty map's marked blocks as byte runs, unmarked; none without a
    /// map.
    #[cfg_attr(not(feature = "wgpu"), allow(dead_code, reason = "used by the GPU raster unit"))]
    pub fn take_dirty_runs(&self) -> Vec<(u64, u64)> {
        if self.dirty.is_null() {
            return Vec::new();
        }
        // SAFETY: the map lives as long as the memory (Vram::dirty).
        unsafe { (*self.dirty).take_runs() }
    }

    #[cfg_attr(not(feature = "wgpu"), allow(dead_code, reason = "used by the GPU raster unit"))]
    pub fn clear_dirty(&self) {
        if !self.dirty.is_null() {
            // SAFETY: as take_dirty_runs().
            unsafe { (*self.dirty).clear() }
        }
    }

    /// The words at byte offset `off` (8-aligned, in range, not wrapping),
    /// `len` bytes of them, for an upload.
    #[cfg_attr(not(feature = "wgpu"), allow(dead_code, reason = "used by the GPU raster unit"))]
    pub fn words_at(&self, off: u64, len: u64) -> &[u64] {
        debug_assert!(off % 8 == 0 && len % 8 == 0 && off + len <= self.size());
        // SAFETY: in range; readers and writers race as the board's units do.
        unsafe { std::slice::from_raw_parts(self.ptr.add((off >> 3) as usize), (len >> 3) as usize) }
    }

    /// Store `words` at byte offset `off` (8-aligned, in range) without
    /// marking the dirty map: a readback from the GPU, which the GPU
    /// already holds.
    ///
    /// It does not mark the memory written: the GPU unit's board always
    /// composes every frame (`DisplayShared::gpu_scanout`), so no flag is
    /// read there.
    #[cfg_attr(not(feature = "wgpu"), allow(dead_code, reason = "used by the GPU raster unit"))]
    pub fn write_words_untracked(&self, off: u64, words: &[u64]) {
        debug_assert!(off % 8 == 0 && off + 8 * words.len() as u64 <= self.size());
        // SAFETY: in range; as above.
        unsafe { std::ptr::copy_nonoverlapping(words.as_ptr(), self.ptr.add((off >> 3) as usize), words.len()) }
        if !self.report.is_null() {
            // SAFETY: the maps live as long as the memory (Vram::dirty).
            unsafe { (*self.maps).mark_range(off, 8 * words.len() as u64, false) }
        }
    }

    /// The runs of memory that changed since the last call (`Vram::report_dirty`).
    pub fn take_reported(&self, running: bool) -> Vec<(u64, u64)> {
        if self.report.is_null() {
            return Vec::new();
        }
        // SAFETY: the maps live as long as the memory (Vram::dirty).
        unsafe { (*self.maps).take_reported(running) }
    }

    /// Bytes the shell wrote into lent memory: the GPU unit's mirror holds
    /// them, so they go up with the next upload.
    #[cfg_attr(not(feature = "wgpu"), allow(dead_code, reason = "used by the GPU raster unit"))]
    pub fn mark_for_upload(&self, off: u64, len: u64) {
        if !self.dirty.is_null() {
            // SAFETY: as above.
            unsafe { (*self.maps).mark_range(off, len, true) }
        }
    }

    /// The memory's size in bytes.
    pub fn size(&self) -> u64 {
        (self.mask as u64 + 1) * 8
    }

    /// The memory as bytes in address order, for what is a byte stream by
    /// nature: a dump file, the memory hash.
    pub fn address_bytes(&self) -> impl Iterator<Item = u8> + '_ {
        self.address_words().flat_map(u64::to_be_bytes)
    }

    /// The memory as words in address order, the lowest address the
    /// highest byte (load()'s bits).
    pub fn address_words(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.size()).step_by(8).map(move |a| self.load(a, 8))
    }

    /// No memory: a view of a field the surface lacks, which nothing reads.
    pub fn null() -> VramPtr {
        VramPtr { ptr: std::ptr::null_mut(), mask: 0, dirty: std::ptr::null(), maps: std::ptr::null(), report: std::ptr::null(), written: &NOWHERE }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The selects the driver sent in the trace of an X start at depth 24 on a
    // 128 MB board (measured.md section 4).
    const FB: [u32; 3] = [0x2000, 0x50, 0x6950];
    const OVERLAY: [u32; 3] = [0x142000, 0x50, 0x20080080];
    const SCRATCH: [u32; 3] = [0x16b800, 0x20, 0x08104851];
    const LEVEL: [u32; 3] = [0x16f800, 0x02, 0x080024d0];

    #[test]
    fn format_words_decode() {
        let f = Format::decode(FB[2]);
        assert_eq!((f.fb_log2, f.win_log2, f.pixel_fmt, f.field_fmt, f.index, f.scratch), (4, 3, 2, 5, false, false));
        let o = Format::decode(OVERLAY[2]);
        assert_eq!((o.fb_log2, o.win_log2, o.pixel_fmt, o.field_fmt, o.index), (1, 1, 1, 0, true));
        assert_eq!(Format::decode(SCRATCH[2]).fb_log2, 3);
        let l = Format::decode(LEVEL[2]);
        assert_eq!((l.fb_log2, l.win_log2, l.field_fmt, l.scratch), (2, 2, 5, true));
    }

    #[test]
    fn frame_buffer_ends_where_the_overlay_starts() {
        let fb = Surface::from_select(&FB);
        let ov = Surface::from_select(&OVERLAY);
        assert_eq!(fb.bytes_per_pixel(), 16);
        assert_eq!(fb.width(), 1280);
        assert_eq!(fb.base as u64 * UNIT + fb.bytes(1024), ov.base as u64 * UNIT);
        // The overlay's 2 bytes a pixel end before the first texture
        // allocation the trace saw.
        assert!(ov.base as u64 * UNIT + ov.bytes(1024) <= 0x16b000 * UNIT);
    }

    #[test]
    fn tiles_are_row_major_and_pixels_row_major_inside() {
        let fb = Surface::from_select(&FB);
        let o = fb.base as u64 * UNIT;
        assert_eq!(fb.offset(0, 0), o);
        assert_eq!(fb.offset(1, 0), o + 16);
        assert_eq!(fb.offset(0, 1), o + 16 * 16);
        assert_eq!(fb.offset(16, 0), o + 256 * 16);
        assert_eq!(fb.offset(0, 16), o + 80 * 256 * 16);
        assert_eq!(fb.offset(1279, 1023), o + fb.bytes(1024) - 16);
    }

    #[test]
    fn fields_are_bit_ranges_of_words() {
        let mut m = Vram::new(1 << 20);
        let p = m.ptr();
        p.store(16, 8, 0x0011_2233_4455_6677);
        assert_eq!(p.load(16, 4), 0x0011_2233);
        assert_eq!(p.load(20, 4), 0x4455_6677);
        assert_eq!(p.load(17, 3), 0x11_2233);
        assert_eq!(p.load(23, 1), 0x77);
        p.store(18, 2, 0xabcd);
        assert_eq!(p.load(16, 8), 0x0011_abcd_4455_6677);
        // Addresses wrap at the memory's end.
        p.store(1 << 20, 4, 0x5a5a_5a5a);
        assert_eq!(p.load(0, 4), 0x5a5a_5a5a);
    }
}
