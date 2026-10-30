//! Ported from include/hw/display/sgi/board-memory.h in jamesbraid/qemu-sgi-origami.
//!
//! SGI graphics board memory and the planes over it. A plane is a typed
//! window (format, size, stride, packing, row origin) over one head's
//! memory; pixel (x, y) is at byte `base + y * stride + x * bpp`. Planes
//! may overlap. Out-of-plane loads read zero and out-of-plane stores are
//! dropped.
//!
//! Adaptations: `Error **errp` is `&mut Option<String>`; the page `host`
//! pointer and a plane run's `host` are `P<u8>` (the family resolves them);
//! `sgi_board_memory_plane` returns a `P<SGIPlane>` (null when absent);
//! (pointer, length) pairs are slices; VMState is dropped.
use super::*;

pub const SGI_PLANE_NONE: u32 = u32::MAX;
pub const SGI_PLANES_MAX: usize = 32;

pub type SGIPlaneFormat = u32;
pub const SGI_PLANE_CI8: SGIPlaneFormat = 0; // 1 byte: colour index
pub const SGI_PLANE_RGB12: SGIPlaneFormat = 1; // 2 bytes: 12-bit colour in a 16-bit word
pub const SGI_PLANE_RGB24: SGIPlaneFormat = 2; // 4 bytes: 24-bit colour in a 32-bit word
pub const SGI_PLANE_RGBA8: SGIPlaneFormat = 3; // 4 bytes
pub const SGI_PLANE_Z24: SGIPlaneFormat = 4; // 4 bytes: 24-bit depth in a 32-bit word
pub const SGI_PLANE_S8: SGIPlaneFormat = 5; // 1 byte: stencil
pub const SGI_PLANE_TAG: SGIPlaneFormat = 6; // 1 byte: per-pixel tag
pub const SGI_PLANE_OVERLAY8: SGIPlaneFormat = 7; // 1 byte
pub const SGI_PLANE_TEXTURE: SGIPlaneFormat = 8; // 4 bytes
pub const SGI_PLANE_RAW: SGIPlaneFormat = 9; // 4 bytes: a word the family interprets

pub type SGIPacking = u32;
pub const SGI_PACK_LE_RED_LOW: SGIPacking = 0;
pub const SGI_PACK_LE_RED_HIGH: SGIPacking = 1;
pub const SGI_PACK_BE_RED_LOW: SGIPacking = 2;
pub const SGI_PACK_BE_RED_HIGH: SGIPacking = 3;

#[derive(Clone, Copy, Default, Debug)]
pub struct SGIPlane {
    pub id: u32, // index in the board's plane table
    pub base: u64,
    pub len: u64, // bytes within the board memory
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: SGIPlaneFormat,
    pub packing: SGIPacking, // what the model writes, labelled
    pub bottom_origin: bool, // plane row 0 is the bottom display line
}

#[derive(Clone, Copy, Default)]
pub struct SGIBoardPage {
    pub host: P<u8>, // null until the family resolves it
    pub guest: u64,  // the page's guest address
}

#[derive(Clone, Default)]
pub struct SGIBoardMemory {
    pub size: u64,
    pub bytes: Vec<u8>, // device-backed, else empty
    pub bytes_len: u32, // size when device-backed, else 0
    pub pages: Vec<SGIBoardPage>, // page-backed, else empty
    pub npages: u32,
    pub page_size: u32,
    /// A fixed table, so a plane index stays valid for the memory's life.
    pub planes: [SGIPlane; SGI_PLANES_MAX],
    pub nplanes: u32,
}

/// Up to 16 inclusive rectangles, in plane coordinates. With n 0 the list
/// is no clip, unless empty is set: then it keeps no pixel.
pub const SGI_CLIP_MAX: usize = 16;

#[derive(Clone, Copy, Default, Debug)]
pub struct SGIClipRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct SGIClipList {
    pub n: u32,
    pub empty: u32, // 0 or 1; with n 0, 1 keeps nothing
    pub r: [SGIClipRect; SGI_CLIP_MAX],
}

// The sixteen logic ops, in OpenGL's order (GL_CLEAR + op).
pub const SGI_LOGIC_CLEAR: u32 = 0;
pub const SGI_LOGIC_AND: u32 = 1;
pub const SGI_LOGIC_AND_REVERSE: u32 = 2;
pub const SGI_LOGIC_COPY: u32 = 3;
pub const SGI_LOGIC_AND_INVERTED: u32 = 4;
pub const SGI_LOGIC_NOOP: u32 = 5;
pub const SGI_LOGIC_XOR: u32 = 6;
pub const SGI_LOGIC_OR: u32 = 7;
pub const SGI_LOGIC_NOR: u32 = 8;
pub const SGI_LOGIC_EQUIV: u32 = 9;
pub const SGI_LOGIC_INVERT: u32 = 10;
pub const SGI_LOGIC_OR_REVERSE: u32 = 11;
pub const SGI_LOGIC_COPY_INVERTED: u32 = 12;
pub const SGI_LOGIC_OR_INVERTED: u32 = 13;
pub const SGI_LOGIC_NAND: u32 = 14;
pub const SGI_LOGIC_SET: u32 = 15;

/// A zeroed device-backed memory of size bytes, at most 4 GiB - 1.
pub fn sgi_board_memory_init(m: &mut SGIBoardMemory, size: u64) {
    // A family constant; the limit is the migration buffer's length.
    assert!(size != 0 && size <= u32::MAX as u64);
    *m = SGIBoardMemory::default();
    m.size = size;
    m.bytes_len = size as u32;
    m.bytes = vec![0u8; size as usize];
}
/// A page-backed memory of npages unresolved pages of page_size bytes.
pub fn sgi_board_memory_init_pages(m: &mut SGIBoardMemory, npages: u32, page_size: u32) {
    assert!(npages != 0 && is_power_of_2(page_size as u64));
    *m = SGIBoardMemory::default();
    m.size = npages as u64 * page_size as u64;
    m.npages = npages;
    m.page_size = page_size;
    m.pages = vec![SGIBoardPage::default(); npages as usize];
}
/// Page i is guest address guest, reached at host (null: unresolved).
pub fn sgi_board_memory_set_page(m: &mut SGIBoardMemory, i: u32, guest: u64, host: P<u8>) {
    assert!(!m.pages.is_empty() && i < m.npages);
    m.pages[i as usize].guest = guest;
    m.pages[i as usize].host = host;
}
pub fn sgi_board_memory_cleanup(m: &mut SGIBoardMemory) {
    *m = SGIBoardMemory::default();
}

pub fn sgi_plane_bytes_per_pixel(format: SGIPlaneFormat) -> u32 {
    sgi_plane_bpp(format)
}

/// Declare a plane. Its id is assigned here and returned; the rest of
/// *plane is copied. Fails, returning SGI_PLANE_NONE (and setting errp),
/// when the plane does not fit the memory, its rows do not fit its stride
/// and length, or the table already holds SGI_PLANES_MAX planes.
pub fn sgi_board_memory_add_plane(
    m: &mut SGIBoardMemory,
    plane: &SGIPlane,
    errp: &mut Option<String>,
) -> u32 {
    let bpp = sgi_plane_bytes_per_pixel(plane.format);
    let row = plane.width as u64 * bpp as u64;

    if plane.width == 0 || plane.height == 0 {
        *errp = Some("plane has no pixels".to_string());
        return SGI_PLANE_NONE;
    }
    if (plane.stride as u64) < row {
        *errp = Some(format!(
            "plane stride {} is shorter than its {}-byte rows",
            plane.stride, row
        ));
        return SGI_PLANE_NONE;
    }
    let need = (plane.height as u64 - 1) * plane.stride as u64 + row;
    if need > plane.len {
        *errp = Some(format!("plane needs {} bytes but has {}", need, plane.len));
        return SGI_PLANE_NONE;
    }
    if plane.base > m.size || plane.len > m.size - plane.base {
        *errp = Some(format!(
            "plane {:#x}+{:#x} is outside {:#x} bytes of board memory",
            plane.base, plane.len, m.size
        ));
        return SGI_PLANE_NONE;
    }

    if m.nplanes as usize == SGI_PLANES_MAX {
        *errp = Some(format!("board memory already has {} planes", SGI_PLANES_MAX));
        return SGI_PLANE_NONE;
    }
    let id = m.nplanes;
    m.nplanes += 1;
    let p = &mut m.planes[id as usize];
    *p = *plane;
    p.id = id;
    crate::dlog!(crate::devlog::LogModule::Odyssey, "sgi_plane_add: plane {} base {:#x} len {:#x} {}x{} stride {} format {}", p.id, p.base, p.len, p.width, p.height, p.stride, p.format as i32);
    p.id
}
/// The plane with this id, or null.
pub fn sgi_board_memory_plane(m: &SGIBoardMemory, id: u32) -> P<SGIPlane> {
    if id < m.nplanes {
        P::from_ref(&m.planes[id as usize])
    } else {
        P::null()
    }
}

#[inline]
pub fn sgi_plane_contains(p: &SGIPlane, x: i32, y: i32) -> bool {
    x >= 0 && y >= 0 && (x as u32) < p.width && (y as u32) < p.height
}

pub fn sgi_clip_contains(clip: &SGIClipList, x: i32, y: i32) -> bool {
    if clip.n == 0 {
        return clip.empty == 0;
    }
    for i in 0..clip.n.min(SGI_CLIP_MAX as u32) {
        let r = &clip.r[i as usize];

        if x >= r.x0 && x <= r.x1 && y >= r.y0 && y <= r.y1 {
            return true;
        }
    }
    false
}
pub fn sgi_logic_op(op: u8, src: u32, dst: u32) -> u32 {
    let s = src;
    let d = dst;
    match (op & 0xf) as u32 {
        SGI_LOGIC_CLEAR => 0,
        SGI_LOGIC_AND => s & d,
        SGI_LOGIC_AND_REVERSE => s & !d,
        SGI_LOGIC_COPY => s,
        SGI_LOGIC_AND_INVERTED => !s & d,
        SGI_LOGIC_NOOP => d,
        SGI_LOGIC_XOR => s ^ d,
        SGI_LOGIC_OR => s | d,
        SGI_LOGIC_NOR => !(s | d),
        SGI_LOGIC_EQUIV => !(s ^ d),
        SGI_LOGIC_INVERT => !d,
        SGI_LOGIC_OR_REVERSE => s | !d,
        SGI_LOGIC_COPY_INVERTED => !s,
        SGI_LOGIC_OR_INVERTED => !s | d,
        SGI_LOGIC_NAND => !(s & d),
        _ => !0u32, // SGI_LOGIC_SET
    }
}

pub fn sgi_plane_load(m: &SGIBoardMemory, p: &SGIPlane, x: i32, y: i32) -> u32 {
    if !sgi_plane_contains(p, x, y) {
        return 0;
    }
    let bpp = sgi_plane_bpp(p.format);
    let at = sgi_plane_offset(p, x, y, bpp);
    if !m.bytes.is_empty() {
        let host = sgi_board_memory_bytes(m, at, bpp);
        return if !host.is_null() {
            sgi_plane_get(p, bpp, unsafe { std::slice::from_raw_parts(host.0, bpp as usize) })
        } else {
            0
        };
    }
    let host = sgi_board_memory_host(m, at, bpp as u64);
    if !host.is_null() {
        return sgi_plane_get(p, bpp, unsafe { std::slice::from_raw_parts(host.0, bpp as usize) });
    }
    let mut bytes = [0u8; 4];
    if !sgi_board_memory_export(m, at, &mut bytes[..bpp as usize]) {
        return 0;
    }
    sgi_plane_get(p, bpp, &bytes)
}
pub fn sgi_plane_store(m: &mut SGIBoardMemory, p: &SGIPlane, x: i32, y: i32, word: u32) {
    if !sgi_plane_contains(p, x, y) {
        return;
    }
    let bpp = sgi_plane_bpp(p.format);
    let at = sgi_plane_offset(p, x, y, bpp);
    if !m.bytes.is_empty() {
        let host = sgi_board_memory_bytes(m, at, bpp);
        if !host.is_null() {
            sgi_plane_put(p, bpp, word, unsafe {
                std::slice::from_raw_parts_mut(host.0, bpp as usize)
            });
        }
        return;
    }
    let host = sgi_board_memory_host(m, at, bpp as u64);
    if !host.is_null() {
        sgi_plane_put(p, bpp, word, unsafe {
            std::slice::from_raw_parts_mut(host.0, bpp as usize)
        });
        return;
    }
    let mut bytes = [0u8; 4];
    sgi_plane_put(p, bpp, word, &mut bytes);
    sgi_board_memory_import(m, at, &bytes[..bpp as usize]);
}
/// Store op(src, dst) at (x, y) through the write mask: bits set in mask
/// take the result, the others keep the plane's word.
pub fn sgi_plane_merge(
    m: &mut SGIBoardMemory,
    p: &SGIPlane,
    x: i32,
    y: i32,
    src: u32,
    op: u8,
    mask: u32,
) {
    let dst = sgi_plane_load(m, p, x, y);
    let val = sgi_logic_op(op, src, dst);

    sgi_plane_store(m, p, x, y, (dst & !mask) | (val & mask));
}

/// A run of pixels along one plane row, reached directly. `host` points
/// into the board memory (or a resolved page) and is valid only until the
/// memory is next changed.
#[derive(Clone, Copy, Default)]
pub struct SGIPlaneRun {
    pub host: P<u8>,
    pub bpp: u32,
    pub le: bool,
}

/// n pixels from (x, y) onwards: fills `run` and returns true when every
/// pixel is inside the plane and one contiguous host range.
pub fn sgi_plane_run(
    m: &SGIBoardMemory,
    p: &SGIPlane,
    x: i32,
    y: i32,
    n: i32,
    run: &mut SGIPlaneRun,
) -> bool {
    if n <= 0 || !sgi_plane_contains(p, x, y) || !sgi_plane_contains(p, x + n - 1, y) {
        return false;
    }
    let bpp = sgi_plane_bpp(p.format);
    let host = sgi_board_memory_host(m, sgi_plane_offset(p, x, y, bpp), n as u64 * bpp as u64);
    if host.is_null() {
        return false;
    }
    *run = SGIPlaneRun { host, bpp, le: sgi_plane_is_le(p) };
    true
}

/// One plane word of `bpp` bytes, little-endian when `le` is set.
#[inline]
pub fn sgi_plane_word_load(bytes: &[u8], bpp: u32, le: bool) -> u32 {
    match bpp {
        1 => bytes[0] as u32,
        2 => {
            if le {
                lduw_le_p(bytes)
            } else {
                lduw_be_p(bytes)
            }
        }
        _ => {
            if le {
                ldl_le_p(bytes)
            } else {
                ldl_be_p(bytes)
            }
        }
    }
}

#[inline]
pub fn sgi_plane_word_store(bytes: &mut [u8], bpp: u32, le: bool, word: u32) {
    match bpp {
        1 => bytes[0] = word as u8,
        2 => {
            if le {
                stw_le_p(bytes, word);
            } else {
                stw_be_p(bytes, word);
            }
        }
        _ => {
            if le {
                stl_le_p(bytes, word);
            } else {
                stl_be_p(bytes, word);
            }
        }
    }
}

#[inline]
pub fn sgi_plane_run_load(run: &SGIPlaneRun, i: i32) -> u32 {
    let b = unsafe {
        std::slice::from_raw_parts(run.host.0.add(i as usize * run.bpp as usize), run.bpp as usize)
    };
    sgi_plane_word_load(b, run.bpp, run.le)
}

#[inline]
pub fn sgi_plane_run_store(run: &SGIPlaneRun, i: i32, word: u32) {
    let b = unsafe {
        std::slice::from_raw_parts_mut(run.host.0.add(i as usize * run.bpp as usize), run.bpp as usize)
    };
    sgi_plane_word_store(b, run.bpp, run.le, word)
}

/// A plane word to and from its stored bytes, in the plane's packing.
pub fn sgi_plane_decode(p: &SGIPlane, bytes: &[u8]) -> u32 {
    sgi_plane_get(p, sgi_plane_bpp(p.format), bytes)
}
pub fn sgi_plane_encode(p: &SGIPlane, word: u32, bytes: &mut [u8]) {
    sgi_plane_put(p, sgi_plane_bpp(p.format), word, bytes)
}

/// Copy dst.len() bytes at base out of board memory. Returns false,
/// copying nothing, when the range is outside the memory or crosses a
/// page the family has not resolved.
pub fn sgi_board_memory_export(m: &SGIBoardMemory, base: u64, dst: &mut [u8]) -> bool {
    sgi_board_memory_copy(m, base, dst.len() as u64, dst.as_mut_ptr(), true)
}
/// The same, into board memory from src.
pub fn sgi_board_memory_import(m: &mut SGIBoardMemory, base: u64, src: &[u8]) -> bool {
    sgi_board_memory_copy(m, base, src.len() as u64, src.as_ptr() as *mut u8, false)
}

// The per-pixel paths take a plane's pixel size once and reuse it.
#[inline]
pub fn sgi_plane_bpp(format: SGIPlaneFormat) -> u32 {
    match format {
        SGI_PLANE_CI8 | SGI_PLANE_S8 | SGI_PLANE_TAG | SGI_PLANE_OVERLAY8 => 1,
        SGI_PLANE_RGB12 => 2,
        SGI_PLANE_RGB24 | SGI_PLANE_RGBA8 | SGI_PLANE_Z24 | SGI_PLANE_TEXTURE | SGI_PLANE_RAW => 4,
        _ => unreachable!(),
    }
}

pub fn sgi_plane_is_le(p: &SGIPlane) -> bool {
    p.packing == SGI_PACK_LE_RED_LOW || p.packing == SGI_PACK_LE_RED_HIGH
}

#[inline]
pub fn sgi_plane_get(p: &SGIPlane, bpp: u32, bytes: &[u8]) -> u32 {
    sgi_plane_word_load(bytes, bpp, sgi_plane_is_le(p))
}

#[inline]
pub fn sgi_plane_put(p: &SGIPlane, bpp: u32, word: u32, bytes: &mut [u8]) {
    sgi_plane_word_store(bytes, bpp, sgi_plane_is_le(p), word)
}

/// The host bytes behind [base, base + len), when they are contiguous on
/// the host: always for a device-backed memory, within one resolved page
/// for a page-backed one. Null otherwise.
pub fn sgi_board_memory_host(m: &SGIBoardMemory, base: u64, len: u64) -> P<u8> {
    if base > m.size || len > m.size - base {
        return P::null();
    }
    if !m.bytes.is_empty() {
        return P(unsafe { m.bytes.as_ptr().add(base as usize) } as *mut u8);
    }
    let in_page = base & (m.page_size as u64 - 1);
    if in_page + len > m.page_size as u64 {
        return P::null();
    }
    let idx = (base / m.page_size as u64) as usize;
    if idx >= m.pages.len() {
        return P::null();
    }
    let host = m.pages[idx].host;
    if host.is_null() {
        P::null()
    } else {
        P(unsafe { host.0.add(in_page as usize) })
    }
}

/// Copy between board memory and buf (a raw pointer so import can take a
/// shared slice): out copies memory to buf, else buf into memory.
pub fn sgi_board_memory_copy(m: &SGIBoardMemory, base: u64, len: u64, buf: *mut u8, out: bool) -> bool {
    let mut done = 0u64;

    if base > m.size || len > m.size - base {
        return false;
    }
    if !m.bytes.is_empty() {
        unsafe {
            let mem = m.bytes.as_ptr().add(base as usize) as *mut u8;
            if out {
                std::ptr::copy_nonoverlapping(mem, buf, len as usize);
            } else {
                std::ptr::copy_nonoverlapping(buf, mem, len as usize);
            }
        }
        return true;
    }
    // Check every page first so a failed copy copies nothing.
    let ps = m.page_size as u64;
    let mut a = base & !(ps - 1);
    while a < base + len {
        if m.pages[(a / ps) as usize].host.is_null() {
            return false;
        }
        a += ps;
    }
    while done < len {
        let at = base + done;
        let n = (len - done).min(ps - (at & (ps - 1)));
        let host = sgi_board_memory_host(m, at, n);

        unsafe {
            if out {
                std::ptr::copy_nonoverlapping(host.0, buf.add(done as usize), n as usize);
            } else {
                std::ptr::copy_nonoverlapping(buf.add(done as usize), host.0, n as usize);
            }
        }
        done += n;
    }
    true
}

pub fn sgi_plane_offset(p: &SGIPlane, x: i32, y: i32, bpp: u32) -> u64 {
    p.base
        .wrapping_add((y as u64).wrapping_mul(p.stride as u64))
        .wrapping_add((x as u64).wrapping_mul(bpp as u64))
}

/// A device-backed memory is one host array, so a pixel inside it is
/// reached directly.
#[inline]
pub fn sgi_board_memory_bytes(m: &SGIBoardMemory, at: u64, bpp: u32) -> P<u8> {
    if at <= m.size && bpp as u64 <= m.size - at {
        P(unsafe { m.bytes.as_ptr().add(at as usize) } as *mut u8)
    } else {
        P::null()
    }
}
