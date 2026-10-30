//! Ported from include/hw/display/sgi/pixel-dma.h in jamesbraid/qemu-sgi-origami.
//!
//! SGI graphics DMA: guest pages resolved to chunks, and pixel transfers
//! between those chunks and a plane.
//!
//! A transfer reads or writes guest memory in chunks of at most a page, as
//! a head's DMA engine bursts. Each chunk is a buffer of its own: a source
//! is read whole when the transfer is mapped, and of a target only the run
//! of bytes the head wrote goes back when it is unmapped, so a target the
//! head did not fully write keeps its other bytes.
use super::*;

use std::sync::Arc;

use crate::traits::{BusDevice, BUS_OK};

/// Guest memory as a graphics head's DMA reaches it: the machine's
/// bus-master view, or none before the machine attaches one.
pub type GuestMemory = Option<Arc<dyn BusDevice>>;

/// Read `buf.len()` guest bytes at `addr`; false when no memory is attached
/// or the bus reports an error.
pub fn read_guest(mem: &GuestMemory, addr: u64, buf: &mut [u8]) -> bool {
    mem.as_deref().is_some_and(|m| crate::physical::read_bytes(m, addr, buf) == BUS_OK)
}

/// Write `buf` to guest memory at `addr`; false as for [`read_guest`].
pub fn write_guest(mem: &GuestMemory, addr: u64, buf: &[u8]) -> bool {
    mem.as_deref().is_some_and(|m| crate::physical::write_bytes(m, addr, buf) == BUS_OK)
}

/// Which way a mapped transfer moves bytes.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum DmaDirection {
    /// Guest memory to the head: chunks are read when mapped.
    #[default]
    ToDevice,
    /// The head to guest memory: written runs go back when unmapped.
    FromDevice,
}

pub const SGI_DMA_CHUNK_MAX: u32 = 4096;
/// No SGI head has a plane taller than this; it bounds a transfer's chunks.
pub const SGI_DMA_MAX_ROWS: u32 = 16384;
/// The smallest page a page list may name.
pub const SGI_DMA_MIN_PAGE_SIZE: u32 = SGI_DMA_CHUNK_MAX;

/// A head's view of guest memory.
#[derive(Clone, Default)]
pub struct DmaBus {
    pub mem: GuestMemory,
    pub ops: P<HeadOps>, // dma_address, dma_chunk, publish
    pub opaque: Opaque,
}

/// A guest buffer as the driver described it: page i at addr[i].
#[derive(Clone, Default)]
pub struct DmaPageList {
    pub addr: Vec<u64>, // device addresses
    pub npages: u32,
    pub page_size: u32,
}

#[derive(Clone, Default)]
pub struct DmaChunk {
    pub host: Vec<u8>, // the chunk's bytes
    pub len: u32,
    pub pos: u64,      // offset within the page-listed buffer
    pub addr: u64,     // device address, as traced
    pub bus_addr: u64, // composed address the transfer used
    pub bounce: bool,
    /// The bytes the head wrote, as offsets into the chunk: one run from
    /// the first to the last, empty while written_end is 0.
    pub written_start: u32,
    pub written_end: u32,
}

#[derive(Clone, Default)]
pub struct DmaMap {
    pub chunks: Vec<DmaChunk>,
    pub cursor: u32, // the chunk the last copy ended in
    pub mem: GuestMemory,
    pub dir: DmaDirection,
}

impl DmaPageList {
    /// The bytes a page list maps, or 0 (logged) when its page size is not a
    /// power of two of at least SGI_DMA_MIN_PAGE_SIZE.
    pub fn bytes(&self) -> u64 {
        if self.page_size < SGI_DMA_MIN_PAGE_SIZE || !(self.page_size as u64).is_power_of_two() {
            crate::dlog!(
                LogModule::Odyssey,
                "sgi-dma: page size {} is not a power of two of at least {}",
                self.page_size,
                SGI_DMA_MIN_PAGE_SIZE
            );
            return 0;
        }
        self.npages as u64 * self.page_size as u64
    }
}

impl DmaBus {
    /// Read buf.len() bytes at device address addr, composed through
    /// dma_address(), tracing them in chunks when `trace` is set. Returns
    /// false, having logged a guest error, when the read fails.
    pub fn read(&self, addr: u64, buf: &mut [u8], trace: bool, trace_seq: u64) -> bool {
        let len = buf.len() as u32;
        let mut done: u32 = 0;

        // No page list here, so chunks break at 4 KiB boundaries of the device
        // address: a traced chunk is then never longer than the contract allows
        // and never crosses a page of any size the families use.
        while done < len {
            let at = addr.wrapping_add(done as u64);
            let n = (len - done).min(SGI_DMA_CHUNK_MAX - (at & (SGI_DMA_CHUNK_MAX as u64 - 1)) as u32);
            let mut bus_addr: u64 = 0;

            if !self.compose(at, &mut bus_addr) {
                return false;
            }
            let dst = &mut buf[done as usize..(done + n) as usize];
            if !read_guest(&self.mem, bus_addr, dst) {
                crate::dlog!(LogModule::Odyssey, "sgi-dma: read of {} bytes at {:#x} failed", n, bus_addr);
                return false;
            }
            self.trace(trace, trace_seq, at, dst);
            done += n;
        }
        true
    }

    /// Resolve [offset, offset + len) of a page-listed buffer and append its
    /// chunks to map. Returns 0, or an Failure after logging a guest error.
    pub fn map(&self, map: &mut DmaMap, pl: &DmaPageList, offset: u64, len: u64, dir: DmaDirection, trace: bool, trace_seq: u64) -> i32 {
        let total = pl.bytes();
        let mut pos = offset;

        assert!(map.chunks.is_empty() || map.dir == dir);
        map.mem = self.mem.clone();
        map.dir = dir;
        let first = map.chunks.len();

        if offset > total || len > total - offset {
            crate::dlog!(
                LogModule::Odyssey,
                "sgi-dma: bytes {:#x}+{:#x} are outside a {}-page buffer",
                offset,
                len,
                pl.npages
            );
            return SGI_FAIL_BOUNDS as i32;
        }

        let ps = pl.page_size as u64;
        while pos < offset + len {
            let page = pl.addr[(pos / ps) as usize];
            let in_page = pos & (ps - 1);
            let addr = page + in_page;
            // The cut at the page's end is what keeps a chunk off guest memory
            // the driver did not hand over: the next page of the buffer is
            // wherever the list says, not the physically next one.
            let n = (offset + len - pos).min(ps - in_page).min(SGI_DMA_CHUNK_MAX as u64);
            let mut bus_addr: u64 = 0;

            if page & (ps - 1) != 0 {
                crate::dlog!(
                    LogModule::Odyssey,
                    "sgi-dma: page {:#x} is not aligned to its {}-byte size",
                    page,
                    pl.page_size
                );
                return SGI_FAIL_BOUNDS as i32;
            }
            if !self.compose(addr, &mut bus_addr) {
                return SGI_FAIL_BUS as i32;
            }
            crate::dlog_dev!(LogModule::Odyssey, "sgi_dma_chunk: addr {:#x} len {}", addr, n);
            let mut c = DmaChunk {
                host: vec![0u8; n as usize],
                len: n as u32,
                pos,
                addr,
                bus_addr,
                bounce: true,
                written_start: 0,
                written_end: 0,
            };
            // Only a source is read. A buffer the head writes starts empty and
            // only the bytes it wrote go back, so a target that is not RAM sees
            // the writes the head made and no read of its own.
            if dir == DmaDirection::ToDevice && !read_guest(&self.mem, bus_addr, &mut c.host[..]) {
                crate::dlog!(LogModule::Odyssey, "sgi-dma: read of {} bytes at {:#x} failed", n, bus_addr);
                return SGI_FAIL_BUS as i32;
            }
            if dir == DmaDirection::ToDevice {
                self.trace(trace, trace_seq, addr, &c.host[..c.len as usize]);
            }
            pos += c.len as u64;
            map.chunks.push(c);
        }
        crate::dlog_dev!(LogModule::Odyssey, "sgi_dma_map: seq {} dir {:?} offset {:#x} len {:#x} chunks {}", trace_seq, dir, offset, len, map.chunks.len() - first);
        0
    }
}

impl DmaMap {
    /// Copy buf.len() bytes at buffer offset pos between map and buf. Returns
    /// false when map does not cover the range. `to_guest`: buf is copied into
    /// map; otherwise map into buf. (buf is `&mut` for both directions.)
    pub fn copy(&mut self, pos: u64, buf: &mut [u8], to_guest: bool) -> bool {
        let len = buf.len() as u64;
        let nchunks = self.chunks.len() as u32;
        let start = if self.cursor < nchunks && self.chunks[self.cursor as usize].pos <= pos {
            self.cursor
        } else {
            0
        };

        // Rows are copied in the order their chunks were mapped, so starting at
        // the chunk the previous copy ended in walks the list about once per
        // transfer instead of once per row. A range the walk from there does
        // not cover is looked for again from the start.
        if self.copy_from(start, pos, buf, to_guest) == len {
            return true;
        }
        start != 0 && self.copy_from(0, pos, buf, to_guest) == len
    }

    /// Release map, writing the head's written runs back to guest memory.
    pub fn unmap(&mut self) {
        for c in self.chunks.iter() {
            let start = c.written_start;
            let len = c.written_end.wrapping_sub(start);

            // Whatever the head wrote reaches the guest, even when the transfer
            // failed later on: the guest's memory then shows how far the
            // transfer got.
            if len != 0 {
                let at = c.bus_addr + start as u64;
                if !write_guest(&self.mem, at, &c.host[start as usize..(start + len) as usize]) {
                    crate::dlog!(LogModule::Odyssey, "sgi-dma: write of {} bytes at {:#x} failed", len, at);
                }
            }
        }
        *self = DmaMap::default();
    }
}

pub type PixelDmaDir = u32;
pub const SGI_DMA_TO_PLANE: PixelDmaDir = 0;
pub const SGI_DMA_FROM_PLANE: PixelDmaDir = 1;

#[derive(Clone, Default)]
pub struct PixelDma {
    pub dir: PixelDmaDir,
    pub plane: u32,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32, // plane coordinates
    pub pitch: u32, // guest bytes from one row to the next
    pub guest_bpp: u8,
    pub conversion: u8, // family constant for convert_row()
    pub logic_op: u8,   // SGI_LOGIC_*, SGI_LOGIC_COPY to store
    pub mask: u32,      // plane bits a store may change
    pub tag_plane: u32, // SGI_PLANE_NONE: no tag written
    pub tag: u8,        // stored in tag_plane per pixel written
    pub clip: ClipList,
    pub trace_chunks: bool,
    pub trace_seq: u64,
    pub seq: u64,    // the completion's sequence number
    pub cookie: u64, // the family's tag for the completion
    pub offset: u64, // buffer offset of row 0
    pub map: DmaMap, // resolved by PixelDma::dma_pixels_map()
}

impl PixelDma {
    /// m is the board memory whose plane the transfer targets, read for the
    /// plane's size only. Returns 0 or an Failure.
    pub fn dma_pixels_map(&mut self, bus: &DmaBus, pl: &DmaPageList, m: &BoardMemory) -> i32 {
        let dir = if self.dir == SGI_DMA_TO_PLANE { DmaDirection::ToDevice } else { DmaDirection::FromDevice };
        let row = self.dma_pixels_row_bytes();

        self.map = DmaMap::default();
        if self.w <= 0 || self.h <= 0 || self.guest_bpp == 0 {
            return 0;
        }
        let mut ret = self.dma_pixels_check(pl, m);
        if ret != 0 {
            return ret;
        }
        // Rows that follow one another map as one range, in fewer chunks.
        if self.pitch as u64 == row {
            return bus.map(&mut self.map, pl, self.offset, row * self.h as u64, dir, self.trace_chunks, self.trace_seq);
        }
        for r in 0..self.h {
            ret = bus.map(
                &mut self.map,
                pl,
                self.offset + r as u64 * self.pitch as u64,
                row,
                dir,
                self.trace_chunks,
                self.trace_seq,
            );
            if ret != 0 {
                return ret;
            }
        }
        0
    }
    /// Host memory only. Returns 0 or an Failure.
    pub fn dma_pixels_run(&mut self, m: &mut BoardMemory, ops: &HeadOps, opaque: Opaque) -> i32 {
        let pp = m.plane(self.plane);
        let mut tag: Option<Plane> = None;
        let row = self.dma_pixels_row_bytes();

        if self.w <= 0 || self.h <= 0 || self.guest_bpp == 0 {
            return 0;
        }
        if pp.is_null() {
            crate::dlog!(LogModule::Odyssey, "sgi-dma: no plane {}", self.plane);
            return SGI_FAIL_BOUNDS as i32;
        }
        let p: Plane = *pp.get();
        if self.tag_plane != SGI_PLANE_NONE {
            let tp = m.plane(self.tag_plane);
            if tp.is_null() {
                crate::dlog!(LogModule::Odyssey, "sgi-dma: no tag plane {}", self.tag_plane);
                return SGI_FAIL_BOUNDS as i32;
            }
            tag = Some(*tp.get());
        }
        // A row wider or a transfer taller than the plane has no destination.
        if self.w as u32 > p.width || self.h as u32 > p.height {
            crate::dlog!(
                LogModule::Odyssey,
                "sgi-dma: {}x{} pixels exceed plane {}'s {}x{}",
                self.w,
                self.h,
                self.plane,
                p.width,
                p.height
            );
            return SGI_FAIL_BOUNDS as i32;
        }
        let pbpp = plane_bytes_per_pixel(p.format);
        if ops.convert_row.is_none() && self.guest_bpp as u32 != pbpp {
            crate::dlog!(
                LogModule::Odyssey,
                "sgi-dma: {}-byte guest pixels to plane {}'s {}-byte pixels need a conversion",
                self.guest_bpp,
                self.plane,
                pbpp
            );
            return SGI_FAIL_FORMAT as i32;
        }

        let mut bytes = vec![0u8; row as usize];
        let mut words = vec![0u32; self.w as usize];
        for r in 0..self.h {
            let pos = self.offset + r as u64 * self.pitch as u64;
            let y = self.y + r;

            if self.dir == SGI_DMA_TO_PLANE {
                if !self.map.copy(pos, &mut bytes, false) {
                    crate::dlog!(LogModule::Odyssey, "sgi-dma: row {} at {:#x} was not mapped", r, pos);
                    return SGI_FAIL_BOUNDS as i32;
                }
                if let Some(convert_row) = ops.convert_row {
                    let wb = unsafe {
                        std::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, words.len() * 4)
                    };
                    convert_row(opaque, self.conversion, true, &bytes, wb, self.w as u32);
                } else {
                    for i in 0..self.w as usize {
                        words[i] = p.decode(&bytes[i * pbpp as usize..]);
                    }
                }
                for i in 0..self.w {
                    let x = self.x + i;

                    if !p.contains(x, y) || !self.clip.contains(x, y) {
                        continue;
                    }
                    m.plane_merge(&p, x, y, words[i as usize], self.logic_op, self.mask);
                    if let Some(t) = &tag {
                        m.plane_store(t, x, y, self.tag as u32);
                    }
                }
            } else {
                for i in 0..self.w {
                    words[i as usize] = m.plane_load(&p, self.x + i, y);
                }
                if let Some(convert_row) = ops.convert_row {
                    let wb = unsafe {
                        std::slice::from_raw_parts(words.as_ptr() as *const u8, words.len() * 4)
                    };
                    convert_row(opaque, self.conversion, false, wb, &mut bytes, self.w as u32);
                } else {
                    for i in 0..self.w as usize {
                        p.encode(words[i], &mut bytes[i * pbpp as usize..]);
                    }
                }
                if !self.map.copy(pos, &mut bytes, true) {
                    crate::dlog!(LogModule::Odyssey, "sgi-dma: row {} at {:#x} was not mapped", r, pos);
                    return SGI_FAIL_BOUNDS as i32;
                }
            }
        }
        0
    }
    /// Release the map and publish WRITTEN with the bytes moved, or FAILED
    /// with status when status is non-zero.
    pub fn dma_pixels_finish(&mut self, status: i32, ops: &HeadOps, opaque: Opaque) {
        let moved: u64 = if self.w > 0 && self.h > 0 {
            self.dma_pixels_row_bytes().wrapping_mul(self.h as u64)
        } else {
            0
        };

        self.map.unmap();
        crate::dlog!(LogModule::Odyssey, "sgi_dma_pixels: seq {} dir {} plane {} at {},{} size {}x{} status {}", self.seq, self.dir as i32, self.plane, self.x, self.y, self.w, self.h, status);
        if status != 0 {
            ops.publish(opaque, self.seq, SGI_DONE_FAILED, self.cookie, status as u64);
        } else {
            ops.publish(opaque, self.seq, SGI_DONE_WRITTEN, self.cookie, moved);
        }
    }
    /// All three steps, for a synchronous head. Returns 0 or an Failure.
    pub fn dma_pixels(&mut self, bus: &DmaBus, pl: &DmaPageList, m: &mut BoardMemory) -> i32 {
        let mut ret = self.dma_pixels_map(bus, pl, m);

        if ret == 0 {
            ret = self.dma_pixels_run(m, bus.ops.get(), bus.opaque);
        }
        self.dma_pixels_finish(ret, bus.ops.get(), bus.opaque);
        ret
    }
}

impl DmaBus {
    pub fn compose(&self, addr: u64, out: &mut u64) -> bool {
        if let Some(ops) = self.ops.as_opt() {
            if let Some(dma_address) = ops.dma_address {
                return dma_address(self.opaque, addr, out);
            }
        }
        *out = addr;
        true
    }

    pub fn trace(&self, trace: bool, seq: u64, addr: u64, bytes: &[u8]) {
        if trace {
            if let Some(ops) = self.ops.as_opt() {
                if let Some(dma_chunk) = ops.dma_chunk {
                    dma_chunk(self.opaque, seq, addr, bytes);
                }
            }
        }
    }
}

impl DmaMap {
    pub fn copy_from(&mut self, start: u32, pos: u64, p: &mut [u8], to_guest: bool) -> u64 {
        let len = p.len() as u64;
        let mut done: u64 = 0;

        let mut i = start as usize;
        while i < self.chunks.len() && done < len {
            let c = &mut self.chunks[i];
            let at = pos + done;

            if at < c.pos || at >= c.pos + c.len as u64 {
                i += 1;
                continue;
            }
            let n = (len - done).min(c.pos + c.len as u64 - at);
            let (d0, n0) = (done as usize, n as usize);
            let off = (at - c.pos) as usize;
            if to_guest {
                let mut from = off as u32;
                let mut to = from + n as u32;

                c.host[off..off + n0].copy_from_slice(&p[d0..d0 + n0]);
                if c.written_end != 0 {
                    from = from.min(c.written_start);
                    to = to.max(c.written_end);
                }
                c.written_start = from;
                c.written_end = to;
            } else {
                p[d0..d0 + n0].copy_from_slice(&c.host[off..off + n0]);
            }
            done += n;
            self.cursor = i as u32;
            i += 1;
        }
        done
    }
}

impl PixelDma {
    pub fn dma_pixels_row_bytes(&self) -> u64 {
        (self.w as i64 as u64).wrapping_mul(self.guest_bpp as u64)
    }

    // Every size here can come from a guest's descriptor, so the transfer must
    // fit its plane and its page list before anything is mapped or allocated.
    pub fn dma_pixels_check(&self, pl: &DmaPageList, m: &BoardMemory) -> i32 {
        let pp = m.plane(self.plane);
        let total = pl.bytes();
        let row = self.dma_pixels_row_bytes();

        if pp.is_null() {
            crate::dlog!(LogModule::Odyssey, "sgi-dma: no plane {}", self.plane);
            return SGI_FAIL_BOUNDS as i32;
        }
        let p: Plane = *pp.get();
        if self.w as u32 > p.width || self.h as u32 > p.height {
            crate::dlog!(
                LogModule::Odyssey,
                "sgi-dma: {}x{} pixels exceed plane {}'s {}x{}",
                self.w,
                self.h,
                self.plane,
                p.width,
                p.height
            );
            return SGI_FAIL_BOUNDS as i32;
        }
        let fits = |h: i32| -> Option<bool> {
            if h as i64 > SGI_DMA_MAX_ROWS as i64 {
                return Some(false);
            }
            let moved = row.checked_mul(h as u64)?;
            if moved > total {
                return Some(false);
            }
            let end = (self.pitch as u64).checked_mul((h - 1) as u64)?;
            let end = end.checked_add(row)?;
            let end = end.checked_add(self.offset)?;
            Some(end <= total)
        };
        if fits(self.h) != Some(true) {
            crate::dlog!(
                LogModule::Odyssey,
                "sgi-dma: {}x{} pixels of {} bytes, pitch {}, from {:#x} do not fit a {}-page buffer",
                self.w,
                self.h,
                self.guest_bpp,
                self.pitch,
                self.offset,
                pl.npages
            );
            return SGI_FAIL_BOUNDS as i32;
        }
        // Overlapping rows are fine to read, but written back through bounce
        // buffers the later chunk's copy of the shared bytes would win.
        if self.dir == SGI_DMA_FROM_PLANE && self.h > 1 && (self.pitch as u64) < row {
            crate::dlog!(LogModule::Odyssey, "sgi-dma: pitch {} overlaps {}-byte rows written to guest memory", self.pitch, row);
            return SGI_FAIL_BOUNDS as i32;
        }
        0
    }
}
