//! Ported from include/hw/display/sgi/pixel-dma.h in jamesbraid/qemu-sgi-origami.
//!
//! SGI graphics DMA: guest pages resolved to chunks, and pixel transfers
//! between those chunks and a plane.
//!
//! Chunk buffers: iris has no dma_memory_map(), so every chunk is the C's
//! "bounce" case. `SGIDMAChunk::host` is an owned `Vec<u8>` of `len` bytes,
//! filled from guest memory at map time for TO_DEVICE, and written back
//! (the `written_start..written_end` run) by sgi_dma_unmap() for FROM_DEVICE.
//! `bounce` is therefore always true. Offsets within a chunk are indices
//! into that Vec. `nchunks`/`capacity` are the Vec's len/capacity and are
//! dropped.
//!
//! Other adaptations: `AddressSpace *as` is an owned `AddressSpace` clone
//! (field `as_`, `as` being a keyword); the page list's `addr` is an owned
//! `Vec<u64>` (npages kept); (void*, len) buffers are slices; `bus->ops` is
//! a `P<SGIHeadOps>`.
use super::*;

pub const SGI_DMA_CHUNK_MAX: u32 = 4096;
/// No SGI head has a plane taller than this; it bounds a transfer's chunks.
pub const SGI_DMA_MAX_ROWS: u32 = 16384;
/// The smallest page a page list may name.
pub const SGI_DMA_MIN_PAGE_SIZE: u32 = SGI_DMA_CHUNK_MAX;

/// A head's view of guest memory.
#[derive(Clone, Default)]
pub struct SGIDMABus {
    pub as_: AddressSpace, // the head's bus address space
    pub ops: P<SGIHeadOps>, // dma_address, dma_chunk, publish
    pub opaque: Opaque,
}

/// A guest buffer as the driver described it: page i at addr[i].
#[derive(Clone, Default)]
pub struct SGIDMAPageList {
    pub addr: Vec<u64>, // device addresses
    pub npages: u32,
    pub page_size: u32,
}

#[derive(Clone, Default)]
pub struct SGIDMAChunk {
    pub host: Vec<u8>, // the chunk's bytes (always a bounce buffer here)
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
pub struct SGIDMAMap {
    pub chunks: Vec<SGIDMAChunk>,
    pub cursor: u32, // the chunk the last copy ended in
    pub as_: AddressSpace,
    pub dir: DMADirection,
}

/// The bytes a page list maps, or 0 (logged) when its page size is not a
/// power of two of at least SGI_DMA_MIN_PAGE_SIZE.
pub fn sgi_dma_page_list_bytes(pl: &SGIDMAPageList) -> u64 {
    if pl.page_size < SGI_DMA_MIN_PAGE_SIZE || !is_power_of_2(pl.page_size as u64) {
        crate::qlog!(
            "sgi-dma: page size {} is not a power of two of at least {}",
            pl.page_size,
            SGI_DMA_MIN_PAGE_SIZE
        );
        return 0;
    }
    pl.npages as u64 * pl.page_size as u64
}

/// Read buf.len() bytes at device address addr, composed through
/// dma_address(), tracing them in chunks when `trace` is set. Returns
/// false, having logged a guest error, when the read fails.
pub fn sgi_dma_read(bus: &SGIDMABus, addr: u64, buf: &mut [u8], trace: bool, trace_seq: u64) -> bool {
    let len = buf.len() as u32;
    let mut done: u32 = 0;

    // No page list here, so chunks break at 4 KiB boundaries of the device
    // address: a traced chunk is then never longer than the contract allows
    // and never crosses a page of any size the families use.
    while done < len {
        let at = addr.wrapping_add(done as u64);
        let n = (len - done).min(SGI_DMA_CHUNK_MAX - (at & (SGI_DMA_CHUNK_MAX as u64 - 1)) as u32);
        let mut bus_addr: u64 = 0;

        if !sgi_dma_compose(bus, at, &mut bus_addr) {
            return false;
        }
        let dst = &mut buf[done as usize..(done + n) as usize];
        let res = dma_memory_read(&bus.as_, bus_addr, dst);
        if res != MEMTX_OK {
            crate::qlog!(
                "sgi-dma: read of {} bytes at {:#x} failed (result {})",
                n,
                bus_addr,
                res
            );
            return false;
        }
        sgi_dma_trace(bus, trace, trace_seq, at, dst);
        done += n;
    }
    true
}

/// Resolve [offset, offset + len) of a page-listed buffer and append its
/// chunks to map. Returns 0, or an SGIFailure after logging a guest error.
pub fn sgi_dma_map(
    bus: &SGIDMABus,
    map: &mut SGIDMAMap,
    pl: &SGIDMAPageList,
    offset: u64,
    len: u64,
    dir: DMADirection,
    trace: bool,
    trace_seq: u64,
) -> i32 {
    let total = sgi_dma_page_list_bytes(pl);
    let mut pos = offset;

    assert!(map.chunks.is_empty() || map.dir == dir);
    map.as_ = bus.as_.clone();
    map.dir = dir;
    let first = map.chunks.len();

    if offset > total || len > total - offset {
        crate::qlog!(
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
            crate::qlog!(
                "sgi-dma: page {:#x} is not aligned to its {}-byte size",
                page,
                pl.page_size
            );
            return SGI_FAIL_BOUNDS as i32;
        }
        if !sgi_dma_compose(bus, addr, &mut bus_addr) {
            return SGI_FAIL_BUS as i32;
        }
        // No dma_memory_map() here: always a bounce buffer.
        crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_dma_bounce: addr {:#x} len {}", addr, n);
        let mut c = SGIDMAChunk {
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
        if dir == DMA_DIRECTION_TO_DEVICE
            && dma_memory_read(&bus.as_, bus_addr, &mut c.host[..]) != MEMTX_OK
        {
            crate::qlog!("sgi-dma: read of {} bytes at {:#x} failed", n, bus_addr);
            return SGI_FAIL_BUS as i32;
        }
        if dir == DMA_DIRECTION_TO_DEVICE {
            sgi_dma_trace(bus, trace, trace_seq, addr, &c.host[..c.len as usize]);
        }
        pos += c.len as u64;
        map.chunks.push(c);
    }
    crate::dlog_dev!(crate::devlog::LogModule::Odyssey, "sgi_dma_map: seq {} dir {} offset {:#x} len {:#x} chunks {}", trace_seq, dir as i32, offset, len, map.chunks.len() - first);
    0
}

/// Copy buf.len() bytes at buffer offset pos between map and buf. Returns
/// false when map does not cover the range. `to_guest`: buf is copied into
/// map; otherwise map into buf. (buf is `&mut` for both directions.)
pub fn sgi_dma_map_copy(map: &mut SGIDMAMap, pos: u64, buf: &mut [u8], to_guest: bool) -> bool {
    let len = buf.len() as u64;
    let nchunks = map.chunks.len() as u32;
    let start = if map.cursor < nchunks && map.chunks[map.cursor as usize].pos <= pos {
        map.cursor
    } else {
        0
    };

    // Rows are copied in the order their chunks were mapped, so starting at
    // the chunk the previous copy ended in walks the list about once per
    // transfer instead of once per row. A range the walk from there does
    // not cover is looked for again from the start.
    if sgi_dma_map_copy_from(map, start, pos, buf, to_guest) == len {
        return true;
    }
    start != 0 && sgi_dma_map_copy_from(map, 0, pos, buf, to_guest) == len
}

/// Release map, writing the head's written runs back to guest memory.
pub fn sgi_dma_unmap(map: &mut SGIDMAMap) {
    for c in map.chunks.iter() {
        let start = c.written_start;
        let len = c.written_end.wrapping_sub(start);

        // Whatever the head wrote reaches the guest, even when the transfer
        // failed later on: the guest's memory then shows how far the
        // transfer got.
        if len != 0 {
            let res = dma_memory_write(
                &map.as_,
                c.bus_addr + start as u64,
                &c.host[start as usize..(start + len) as usize],
            );

            if res != MEMTX_OK {
                crate::qlog!(
                    "sgi-dma: write of {} bytes at {:#x} failed (result {})",
                    len,
                    c.bus_addr + start as u64,
                    res
                );
            }
        }
    }
    *map = SGIDMAMap::default();
}

pub type SGIPixelDMADir = u32;
pub const SGI_DMA_TO_PLANE: SGIPixelDMADir = 0;
pub const SGI_DMA_FROM_PLANE: SGIPixelDMADir = 1;

#[derive(Clone, Default)]
pub struct SGIPixelDMA {
    pub dir: SGIPixelDMADir,
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
    pub clip: SGIClipList,
    pub trace_chunks: bool,
    pub trace_seq: u64,
    pub seq: u64,    // the completion's sequence number
    pub cookie: u64, // the family's tag for the completion
    pub offset: u64, // buffer offset of row 0
    pub map: SGIDMAMap, // resolved by sgi_dma_pixels_map()
}

/// m is the board memory whose plane the transfer targets, read for the
/// plane's size only. Returns 0 or an SGIFailure.
pub fn sgi_dma_pixels_map(
    d: &mut SGIPixelDMA,
    bus: &SGIDMABus,
    pl: &SGIDMAPageList,
    m: &SGIBoardMemory,
) -> i32 {
    let dir = if d.dir == SGI_DMA_TO_PLANE { DMA_DIRECTION_TO_DEVICE } else { DMA_DIRECTION_FROM_DEVICE };
    let row = sgi_dma_pixels_row_bytes(d);

    d.map = SGIDMAMap::default();
    if d.w <= 0 || d.h <= 0 || d.guest_bpp == 0 {
        return 0;
    }
    let mut ret = sgi_dma_pixels_check(d, pl, m);
    if ret != 0 {
        return ret;
    }
    // Rows that follow one another map as one range, in fewer chunks.
    if d.pitch as u64 == row {
        return sgi_dma_map(bus, &mut d.map, pl, d.offset, row * d.h as u64, dir, d.trace_chunks, d.trace_seq);
    }
    for r in 0..d.h {
        ret = sgi_dma_map(
            bus,
            &mut d.map,
            pl,
            d.offset + r as u64 * d.pitch as u64,
            row,
            dir,
            d.trace_chunks,
            d.trace_seq,
        );
        if ret != 0 {
            return ret;
        }
    }
    0
}
/// Host memory only. Returns 0 or an SGIFailure.
pub fn sgi_dma_pixels_run(
    d: &mut SGIPixelDMA,
    m: &mut SGIBoardMemory,
    ops: &SGIHeadOps,
    opaque: Opaque,
) -> i32 {
    let pp = sgi_board_memory_plane(m, d.plane);
    let mut tag: Option<SGIPlane> = None;
    let row = sgi_dma_pixels_row_bytes(d);

    if d.w <= 0 || d.h <= 0 || d.guest_bpp == 0 {
        return 0;
    }
    if pp.is_null() {
        crate::qlog!("sgi-dma: no plane {}", d.plane);
        return SGI_FAIL_BOUNDS as i32;
    }
    let p: SGIPlane = *pp.get();
    if d.tag_plane != SGI_PLANE_NONE {
        let tp = sgi_board_memory_plane(m, d.tag_plane);
        if tp.is_null() {
            crate::qlog!("sgi-dma: no tag plane {}", d.tag_plane);
            return SGI_FAIL_BOUNDS as i32;
        }
        tag = Some(*tp.get());
    }
    // A row wider or a transfer taller than the plane has no destination.
    if d.w as u32 > p.width || d.h as u32 > p.height {
        crate::qlog!(
            "sgi-dma: {}x{} pixels exceed plane {}'s {}x{}",
            d.w,
            d.h,
            d.plane,
            p.width,
            p.height
        );
        return SGI_FAIL_BOUNDS as i32;
    }
    let pbpp = sgi_plane_bytes_per_pixel(p.format);
    if ops.convert_row.is_none() && d.guest_bpp as u32 != pbpp {
        crate::qlog!(
            "sgi-dma: {}-byte guest pixels to plane {}'s {}-byte pixels need a conversion",
            d.guest_bpp,
            d.plane,
            pbpp
        );
        return SGI_FAIL_FORMAT as i32;
    }

    let mut bytes = vec![0u8; row as usize];
    let mut words = vec![0u32; d.w as usize];
    for r in 0..d.h {
        let pos = d.offset + r as u64 * d.pitch as u64;
        let y = d.y + r;

        if d.dir == SGI_DMA_TO_PLANE {
            if !sgi_dma_map_copy(&mut d.map, pos, &mut bytes, false) {
                crate::qlog!("sgi-dma: row {} at {:#x} was not mapped", r, pos);
                return SGI_FAIL_BOUNDS as i32;
            }
            if let Some(convert_row) = ops.convert_row {
                let wb = unsafe {
                    std::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, words.len() * 4)
                };
                convert_row(opaque, d.conversion, true, &bytes, wb, d.w as u32);
            } else {
                for i in 0..d.w as usize {
                    words[i] = sgi_plane_decode(&p, &bytes[i * pbpp as usize..]);
                }
            }
            for i in 0..d.w {
                let x = d.x + i;

                if !sgi_plane_contains(&p, x, y) || !sgi_clip_contains(&d.clip, x, y) {
                    continue;
                }
                sgi_plane_merge(m, &p, x, y, words[i as usize], d.logic_op, d.mask);
                if let Some(t) = &tag {
                    sgi_plane_store(m, t, x, y, d.tag as u32);
                }
            }
        } else {
            for i in 0..d.w {
                words[i as usize] = sgi_plane_load(m, &p, d.x + i, y);
            }
            if let Some(convert_row) = ops.convert_row {
                let wb = unsafe {
                    std::slice::from_raw_parts(words.as_ptr() as *const u8, words.len() * 4)
                };
                convert_row(opaque, d.conversion, false, wb, &mut bytes, d.w as u32);
            } else {
                for i in 0..d.w as usize {
                    sgi_plane_encode(&p, words[i], &mut bytes[i * pbpp as usize..]);
                }
            }
            if !sgi_dma_map_copy(&mut d.map, pos, &mut bytes, true) {
                crate::qlog!("sgi-dma: row {} at {:#x} was not mapped", r, pos);
                return SGI_FAIL_BOUNDS as i32;
            }
        }
    }
    0
}
/// Release the map and publish WRITTEN with the bytes moved, or FAILED
/// with status when status is non-zero.
pub fn sgi_dma_pixels_finish(d: &mut SGIPixelDMA, status: i32, ops: &SGIHeadOps, opaque: Opaque) {
    let moved: u64 = if d.w > 0 && d.h > 0 {
        sgi_dma_pixels_row_bytes(d).wrapping_mul(d.h as u64)
    } else {
        0
    };

    sgi_dma_unmap(&mut d.map);
    crate::dlog!(crate::devlog::LogModule::Odyssey, "sgi_dma_pixels: seq {} dir {} plane {} at {},{} size {}x{} status {}", d.seq, d.dir as i32, d.plane, d.x, d.y, d.w, d.h, status);
    if status != 0 {
        sgi_head_publish(ops, opaque, d.seq, SGI_DONE_FAILED, d.cookie, status as u64);
    } else {
        sgi_head_publish(ops, opaque, d.seq, SGI_DONE_WRITTEN, d.cookie, moved);
    }
}
/// All three steps, for a synchronous head. Returns 0 or an SGIFailure.
pub fn sgi_dma_pixels(
    d: &mut SGIPixelDMA,
    bus: &SGIDMABus,
    pl: &SGIDMAPageList,
    m: &mut SGIBoardMemory,
) -> i32 {
    let mut ret = sgi_dma_pixels_map(d, bus, pl, m);

    if ret == 0 {
        ret = sgi_dma_pixels_run(d, m, bus.ops.get(), bus.opaque);
    }
    sgi_dma_pixels_finish(d, ret, bus.ops.get(), bus.opaque);
    ret
}

pub fn sgi_dma_compose(bus: &SGIDMABus, addr: u64, out: &mut u64) -> bool {
    if let Some(ops) = bus.ops.as_opt() {
        if let Some(dma_address) = ops.dma_address {
            return dma_address(bus.opaque, addr, out);
        }
    }
    *out = addr;
    true
}

pub fn sgi_dma_trace(bus: &SGIDMABus, trace: bool, seq: u64, addr: u64, bytes: &[u8]) {
    if trace {
        if let Some(ops) = bus.ops.as_opt() {
            if let Some(dma_chunk) = ops.dma_chunk {
                dma_chunk(bus.opaque, seq, addr, bytes);
            }
        }
    }
}

pub fn sgi_dma_map_copy_from(map: &mut SGIDMAMap, start: u32, pos: u64, p: &mut [u8], to_guest: bool) -> u64 {
    let len = p.len() as u64;
    let mut done: u64 = 0;

    let mut i = start as usize;
    while i < map.chunks.len() && done < len {
        let c = &mut map.chunks[i];
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
        map.cursor = i as u32;
        i += 1;
    }
    done
}

pub fn sgi_dma_pixels_row_bytes(d: &SGIPixelDMA) -> u64 {
    (d.w as i64 as u64).wrapping_mul(d.guest_bpp as u64)
}

// Every size here can come from a guest's descriptor, so the transfer must
// fit its plane and its page list before anything is mapped or allocated.
pub fn sgi_dma_pixels_check(d: &SGIPixelDMA, pl: &SGIDMAPageList, m: &SGIBoardMemory) -> i32 {
    let pp = sgi_board_memory_plane(m, d.plane);
    let total = sgi_dma_page_list_bytes(pl);
    let row = sgi_dma_pixels_row_bytes(d);

    if pp.is_null() {
        crate::qlog!("sgi-dma: no plane {}", d.plane);
        return SGI_FAIL_BOUNDS as i32;
    }
    let p: SGIPlane = *pp.get();
    if d.w as u32 > p.width || d.h as u32 > p.height {
        crate::qlog!(
            "sgi-dma: {}x{} pixels exceed plane {}'s {}x{}",
            d.w,
            d.h,
            d.plane,
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
        let end = (d.pitch as u64).checked_mul((h - 1) as u64)?;
        let end = end.checked_add(row)?;
        let end = end.checked_add(d.offset)?;
        Some(end <= total)
    };
    if fits(d.h) != Some(true) {
        crate::qlog!(
            "sgi-dma: {}x{} pixels of {} bytes, pitch {}, from {:#x} do not fit a {}-page buffer",
            d.w,
            d.h,
            d.guest_bpp,
            d.pitch,
            d.offset,
            pl.npages
        );
        return SGI_FAIL_BOUNDS as i32;
    }
    // Overlapping rows are fine to read, but written back through bounce
    // buffers the later chunk's copy of the shared bytes would win.
    if d.dir == SGI_DMA_FROM_PLANE && d.h > 1 && (d.pitch as u64) < row {
        crate::qlog!("sgi-dma: pitch {} overlaps {}-byte rows written to guest memory", d.pitch, row);
        return SGI_FAIL_BOUNDS as i32;
    }
    0
}
