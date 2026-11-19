//! The back end's bulk state in the chunk store, for a snapshot.
//!
//! Every allocated word array of the pixel store (the colour, depth, stencil and
//! the other words of a pixel's record, and the display-ID, overlay and popup
//! planes beside them) and the cursor's glyph memory are written as 64 KiB chunks
//! of big-endian words. A chunk of all zeros is not written: its place in the
//! list holds `ZERO`, so a mostly empty array costs a list of markers. An
//! allocated array that is all zero still gets its entry, so a restored store
//! allocates what the saved one had (the store's hash tells an unallocated array
//! from a zero one).
//!
//! Not saved here: the tile ring and the draw-state table (the board is
//! quiesced, so both are empty), the composed frame and the baked display tables
//! (rebuilt from the tables as loaded). The small state (the pipe's registers,
//! the display tables, the contexts) is the machine's `Saveable` part, which
//! this module does not cover. A snapshot loads only into the build that saved it.

use super::display::tables::Tables;
use super::store::texture::{Page, PAGE_WORDS, PLANES};
use super::store::{Drained, Store, StoreOp, POSITIONS};
use crate::chunk_store::{ChunkHash, ChunkStore, CHUNK_SIZE};
use serde::{Deserialize, Serialize};
use std::io;

/// The hash a zero chunk's place holds; the store never holds a chunk by it.
pub const ZERO: ChunkHash = [0; 32];

/// One saved memory: its name and its chunks, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KonaChunk {
    pub name: String,
    pub hashes: Vec<ChunkHash>,
}

const WORDS: usize = CHUNK_SIZE / 4;

/// Bytes as chunks, with a zero chunk's place holding `ZERO`.
pub fn put_bytes(cs: &ChunkStore, bytes: &[u8]) -> io::Result<Vec<ChunkHash>> {
    bytes.chunks(CHUNK_SIZE).map(|c| if c.iter().all(|&b| b == 0) { Ok(ZERO) } else { cs.put(c) }).collect()
}

/// What `put_bytes` wrote, `len` bytes of it.
pub fn get_bytes(cs: &ChunkStore, hashes: &[ChunkHash], len: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(len);
    for (i, h) in hashes.iter().enumerate() {
        let want = (len.saturating_sub(i * CHUNK_SIZE)).min(CHUNK_SIZE);
        if *h == ZERO {
            out.resize(out.len() + want, 0);
        } else {
            let b = cs.get(h).map_err(|e| format!("chunk {i}: {e}"))?;
            if b.len() != want {
                return Err(format!("chunk {i} holds {} bytes, not {want}", b.len()));
            }
            out.extend_from_slice(&b);
        }
    }
    if out.len() != len {
        return Err(format!("{} bytes saved, {len} expected", out.len()));
    }
    Ok(out)
}

fn put(cs: &ChunkStore, words: &[u32]) -> io::Result<Vec<ChunkHash>> {
    let mut bytes = Vec::with_capacity(CHUNK_SIZE);
    words
        .chunks(WORDS)
        .map(|c| {
            if c.iter().all(|&w| w == 0) {
                return Ok(ZERO);
            }
            bytes.clear();
            c.iter().for_each(|w| bytes.extend_from_slice(&w.to_be_bytes()));
            cs.put(&bytes)
        })
        .collect()
}

fn get(cs: &ChunkStore, hashes: &[ChunkHash], len: usize) -> Result<Vec<u32>, String> {
    // A manifest is data: more or fewer chunks than the array needs is refused before any is read.
    if hashes.len() != len.div_ceil(WORDS) {
        return Err(format!("{} chunks for {len} words", hashes.len()));
    }
    let mut words = Vec::with_capacity(len);
    for (i, h) in hashes.iter().enumerate() {
        let want = (len - i * WORDS).min(WORDS);
        if *h == ZERO {
            words.resize(words.len() + want, 0);
            continue;
        }
        let bytes = cs.get(h).map_err(|e| format!("chunk {i}: {e}"))?;
        if bytes.len() != want * 4 {
            return Err(format!("chunk {i} holds {} bytes, not {}", bytes.len(), want * 4));
        }
        words.extend(bytes.chunks_exact(4).map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]])));
    }
    if words.len() != len {
        return Err(format!("{} words saved, {len} expected", words.len()));
    }
    Ok(words)
}

/// Save the store's allocated arrays and the cursor memory.
pub fn save(store: &Store, tables: &Tables, cs: &ChunkStore) -> io::Result<Vec<KonaChunk>> {
    let mut out = Vec::new();
    store.observe_all();
    let store = store.cpu();
    for w in store.allocated() {
        let a = store.array(w).expect("allocated");
        // SAFETY: `i` is below the array's length.
        let words: Vec<u32> = (0..a.len()).map(|i| unsafe { a.load(i) }).collect();
        out.push(KonaChunk { name: format!("pixel-{w}"), hashes: put(cs, &words)? });
    }
    out.push(KonaChunk { name: "cursor-ram".into(), hashes: put(cs, tables.cursor_words())? });
    // Texture memory: each plane that holds pages, by its number and the length of its page table (both in the
    // name), one chunk to a page and a zero marker where the table has no page. A page is one chunk long.
    let tex = store.texture();
    for (plane, pages) in tex.used() {
        let hashes = (0..pages)
            .map(|pg| match tex.page(plane, pg) {
                Some(p) => Ok(put(cs, p)?.remove(0)),
                None => Ok(ZERO),
            })
            .collect::<io::Result<Vec<_>>>()?;
        out.push(KonaChunk { name: format!("texture-g{plane}-{pages}"), hashes });
    }
    Ok(out)
}

/// What a streamed snapshot's pieces fill in, sent ahead of them: zero pieces are not sent, so the
/// arrays, the cursor memory and the texture planes are named here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    pub entries: u32,
    pub arrays: Vec<u16>,
    pub cursor_words: u32,
    /// Each plane holding pages, with the length of its page table.
    pub planes: Vec<(u16, u32)>,
}

pub fn layout(store: &Store, tables: &Tables) -> Layout {
    let cpu = store.cpu();
    Layout {
        entries: cpu.width() * cpu.height(),
        arrays: cpu.allocated().into_iter().map(|w| w as u16).collect(),
        cursor_words: tables.cursor_words().len() as u32,
        planes: cpu.texture().used().into_iter().map(|(p, n)| (p, n as u32)).collect(),
    }
}

/// Hand `put` each non-zero piece of the bulk state (name, piece number, big-endian bytes), one piece
/// in memory at a time: `pixel-W` and `cursor-ram` in `CHUNK_SIZE` pieces, `texture-gP` a page a piece.
pub fn save_stream(store: &Store, tables: &Tables, put: &mut dyn FnMut(&str, u64, Vec<u8>) -> Result<(), String>) -> Result<(), String> {
    store.observe_all();
    let store = store.cpu();
    let mut emit = |name: &str, index: usize, words: &mut dyn Iterator<Item = u32>| {
        let bytes: Vec<u8> = words.flat_map(u32::to_be_bytes).collect();
        if bytes.iter().all(|&b| b == 0) { Ok(()) } else { put(name, index as u64, bytes) }
    };
    for w in store.allocated() {
        let a = store.array(w).expect("allocated");
        for i in 0..a.len().div_ceil(WORDS) {
            // SAFETY: `k` is below the array's length.
            emit(&format!("pixel-{w}"), i, &mut (i * WORDS..(i * WORDS + WORDS).min(a.len())).map(|k| unsafe { a.load(k) }))?;
        }
    }
    let cursor = tables.cursor_words();
    for (i, c) in cursor.chunks(WORDS).enumerate() {
        emit("cursor-ram", i, &mut c.iter().copied())?;
    }
    let tex = store.texture();
    for (plane, pages) in tex.used() {
        for pg in 0..pages {
            if let Some(p) = tex.page(plane, pg) {
                emit(&format!("texture-g{plane}"), pg, &mut p.iter().copied())?;
            }
        }
    }
    Ok(())
}

/// A streamed snapshot's bulk state, filled in as pieces arrive and checked as they do.
pub struct Loader {
    entries: usize,
    data: Vec<(usize, Vec<u32>)>,
    cursor: Vec<u32>,
    planes: Vec<(u16, Vec<Option<Box<Page>>>)>,
}

impl Loader {
    /// Check `layout` against a store and tables of this configuration, and make room for its pieces.
    pub fn new(store: &Store, tables: &Tables, layout: &Layout) -> Result<Loader, String> {
        let store = store.cpu();
        let entries = store.width() as usize * store.height() as usize;
        if layout.entries as usize != entries {
            return Err(format!("the snapshot's store holds {} pixels, this one {entries}", layout.entries));
        }
        if layout.cursor_words as usize != tables.cursor_words().len() {
            return Err("the snapshot's cursor memory is another size".into());
        }
        let mut data = Vec::new();
        for &w in &layout.arrays {
            if w as usize >= POSITIONS || data.iter().any(|&(q, _)| q == w as usize) {
                return Err(format!("the layout names word position {w} twice or past the store"));
            }
            data.push((w as usize, vec![0; entries]));
        }
        let mut planes: Vec<(u16, Vec<Option<Box<Page>>>)> = Vec::new();
        for &(p, n) in &layout.planes {
            if p as usize >= PLANES || planes.iter().any(|&(q, _)| q == p) || n as usize > store.texture().plane_pages() {
                return Err(format!("the layout names texture plane {p} twice, past the planes or longer than a bank"));
            }
            planes.push((p, (0..n).map(|_| None).collect()));
        }
        Ok(Loader { entries, data, cursor: vec![0; tables.cursor_words().len()], planes })
    }

    /// Take one piece: where it goes and how long it is are checked against the layout.
    pub fn put(&mut self, name: &str, index: u64, bytes: &[u8]) -> Result<(), String> {
        let bad = |why: &str| Err(format!("{name} piece {index}: {why}"));
        let words = |bytes: &[u8]| -> Vec<u32> { bytes.chunks_exact(4).map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]])).collect() };
        let at = |len: usize| (index as usize).checked_mul(WORDS).filter(|&a| a < len);
        let want = |at: usize, len: usize| (len - at).min(WORDS) * 4;
        if let Some(Ok(w)) = name.strip_prefix("pixel-").map(str::parse::<usize>) {
            let Some((_, a)) = self.data.iter_mut().find(|(q, _)| *q == w) else { return bad("no such array in the layout") };
            let Some(at) = at(self.entries).filter(|&at| want(at, self.entries) == bytes.len()) else { return bad("out of range or the wrong length") };
            a[at..at + bytes.len() / 4].copy_from_slice(&words(bytes));
        } else if name == "cursor-ram" {
            let Some(at) = at(self.cursor.len()).filter(|&at| want(at, self.cursor.len()) == bytes.len()) else { return bad("out of range or the wrong length") };
            self.cursor[at..at + bytes.len() / 4].copy_from_slice(&words(bytes));
        } else if let Some(Ok(p)) = name.strip_prefix("texture-g").map(str::parse::<u16>) {
            let Some((_, pages)) = self.planes.iter_mut().find(|(q, _)| *q == p) else { return bad("no such plane in the layout") };
            let Some(slot) = pages.get_mut(index as usize) else { return bad("past the plane's page table") };
            if bytes.len() != PAGE_WORDS * 4 {
                return bad("not one page long");
            }
            *slot = Some(Box::<Page>::try_from(words(bytes).into_boxed_slice()).map_err(|_| "a page is not PAGE_WORDS long".to_string())?);
        } else {
            return Err(format!("unknown piece {name}"));
        }
        Ok(())
    }

    /// The state, once every piece is in: refused when it holds more texture pages than the product's memory does.
    pub fn finish(self, store: &Store) -> Result<Prepared, String> {
        let store = store.cpu();
        let held: usize = self.planes.iter().map(|(_, p)| p.iter().flatten().count()).sum();
        if held > store.texture().cap_pages() {
            return Err(format!("{held} texture pages saved, past the {} the product's memory holds", store.texture().cap_pages()));
        }
        Ok(Prepared { data: self.data, cursor: self.cursor, planes: self.planes })
    }
}

/// A snapshot's bulk state read into memory and checked, ready to apply.
pub struct Prepared {
    data: Vec<(usize, Vec<u32>)>,
    cursor: Vec<u32>,
    /// Texture memory's planes, by number.
    planes: Vec<(u16, Vec<Option<Box<Page>>>)>,
}

/// Read and check every chunk `save` wrote for a store and tables of the same configuration, and
/// change nothing.
pub fn prepare(store: &Store, tables: &Tables, cs: &ChunkStore, list: &[KonaChunk]) -> Result<Prepared, String> {
    let mut arrays = Vec::new();
    let mut cursor = None;
    let mut texture = Vec::new();
    let store = store.cpu();
    for c in list {
        if let Some((p, n)) = c.name.strip_prefix("texture-g").and_then(|r| r.split_once('-')).and_then(|(p, n)| Some((p.parse::<u16>().ok()?, n.parse::<usize>().ok()?))) {
            if p as usize >= PLANES || texture.iter().any(|&(q, _, _)| q == p) {
                return Err(format!("chunk {} names no texture plane or repeats one", c.name));
            }
            if n > store.texture().plane_pages() || c.hashes.len() != n {
                return Err(format!("{}: the saved plane is past a bank of one raster manager's memory", c.name));
            }
            texture.push((p, n, c));
            continue;
        }
        match c.name.strip_prefix("pixel-").map(str::parse::<usize>) {
            Some(Ok(w)) if w < POSITIONS => arrays.push((w, c)),
            Some(_) => return Err(format!("chunk {} names no word position", c.name)),
            None if c.name == "cursor-ram" => cursor = Some(c),
            None => return Err(format!("unknown chunk {}", c.name)),
        }
    }
    let entries = store.width() as usize * store.height() as usize;
    let mut data = Vec::new();
    for (w, c) in arrays {
        data.push((w, get(cs, &c.hashes, entries).map_err(|e| format!("{}: {e}", c.name))?));
    }
    let cursor = match cursor {
        Some(c) => get(cs, &c.hashes, tables.cursor_words().len()).map_err(|e| format!("cursor-ram: {e}"))?,
        None => vec![0; tables.cursor_words().len()],
    };
    let mut planes = Vec::new();
    let mut held = 0;
    for (p, n, c) in texture {
        let mut pages = Vec::with_capacity(n);
        for (i, h) in c.hashes.iter().enumerate() {
            if *h == ZERO {
                pages.push(None);
                continue;
            }
            let words = get(cs, std::slice::from_ref(h), PAGE_WORDS).map_err(|e| format!("{} page {i}: {e}", c.name))?;
            pages.push(Some(Box::<Page>::try_from(words.into_boxed_slice()).map_err(|_| "a page is not PAGE_WORDS long".to_string())?));
            held += 1;
        }
        planes.push((p, pages));
    }
    if held > store.texture().cap_pages() {
        return Err(format!("{held} texture pages saved, past the {} the product's memory holds", store.texture().cap_pages()));
    }
    Ok(Prepared { data, cursor, planes })
}

/// Put what `prepare` read into the store and tables: every array the snapshot names is allocated
/// and filled, and the others are zeroed, so the store reads as it did when saved.
pub fn apply(store: &Store, drained: Drained, tables: &mut Tables, p: Prepared) -> Result<(), String> {
    store.apply(StoreOp::Reset(drained));
    // SAFETY: `drained` says no tile worker runs, and nothing draws until this returns.
    let mut texture = unsafe { store.cpu().texture().exclusive() };
    for (plane, pages) in p.planes {
        if !texture.set_pages(plane, pages) {
            return Err(format!("texture plane {plane}: refused"));
        }
    }
    for (w, words) in p.data {
        store.apply(StoreOp::Restore { word: w, words: &words, drained });
    }
    tables.load_cursor_words(&p.cursor);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::kona::back::store::{DID_PLANE, POPUP_PLANE};
    use crate::dev::kona::back::KonaBackend;
    use crate::dev::kona::KonaConfig;
    use crate::test_temp::TempPath;

    /// A chunk store's directory that goes with the test, however it ends.
    fn dir(tag: &str) -> TempPath {
        TempPath::dir(&format!("kona-snap-{tag}"))
    }

    fn board() -> KonaBackend {
        KonaBackend::new(KonaConfig { managed: (200, 100), ..KonaConfig::default() })
    }

    /// A board with a few arrays drawn on: colour words, the ID plane, and a zero array.
    fn drawn() -> KonaBackend {
        let b = board();
        for (w, v) in [(1usize, 0xff12_3456u32), (2, 0xff65_4321)] {
            b.store.write(7, 9, w, v);
            b.store.write(199, 99, w, v ^ 1);
        }
        b.store.write(3, 3, DID_PLANE, 13 << 2);
        b.store.ensure(POPUP_PLANE);
        b
    }

    /// Save, load into a fresh board, save again: the store reads the same, the
    /// arrays allocated are the same (an allocated zero array too), and the second save
    /// writes the same chunk lists.
    #[test]
    fn a_saved_store_restores_into_a_fresh_board_and_saves_the_same() {
        let d = dir("round");
        let cs = ChunkStore::new(&*d);
        let mut a = drawn();
        let first = a.save_chunks(&cs).unwrap();
        let mut b = board();
        b.load_chunks(&cs, &first).unwrap();
        assert_eq!(b.store.checksum(), a.store.checksum());
        assert_eq!(b.store.allocated(), a.store.allocated());
        assert_eq!(b.store.read(7, 9, 1), 0xff12_3456);
        assert_eq!(b.save_chunks(&cs).unwrap(), first, "save, load, save");
    }

    /// The streamed form: zero pieces are not sent, the layout names every array and plane, and the
    /// pieces restore a fresh board to the same store and texture memory.
    #[test]
    fn a_streamed_snapshot_restores_into_a_fresh_board() {
        let mut a = drawn();
        assert!(a.store.texture().ex().set(2, 70_000, 0xff0a_0b0c));
        let layout = a.bulk_layout();
        let mut pieces = Vec::new();
        a.save_stream(&mut |n, i, b| Ok(pieces.push((n.to_string(), i, b)))).unwrap();
        assert!(layout.arrays.contains(&(POPUP_PLANE as u16)), "an all-zero array is named");
        assert!(pieces.iter().all(|(n, _, b)| n != &format!("pixel-{POPUP_PLANE}") && b.iter().any(|&x| x != 0)), "and sends nothing");
        let mut b = board();
        let mut load = b.bulk_loader(&layout).unwrap();
        for (n, i, bytes) in &pieces {
            load.put(n, *i, bytes).unwrap();
        }
        let prepared = b.finish_loader(load).unwrap();
        b.apply_chunks(prepared).unwrap();
        assert_eq!((b.store.checksum(), b.store.allocated()), (a.store.checksum(), a.store.allocated()));
        assert_eq!(b.store.texture().get(2, 70_000), 0xff0a_0b0c);
        // A piece the layout does not name, or of the wrong length, is refused.
        let mut load = board().bulk_loader(&layout).unwrap();
        assert!(load.put("pixel-9", 0, &[0; 4]).is_err() && load.put("pixel-1", 0, &[0; 4]).is_err() && load.put("pixel-1", 1 << 40, &[]).is_err());
    }

    /// A chunk of zeros is not written, and its place holds the zero hash.
    #[test]
    fn zero_chunks_are_not_stored() {
        let d = dir("zero");
        let cs = ChunkStore::new(&*d);
        let mut a = drawn();
        let list = a.save_chunks(&cs).unwrap();
        let popup = list.iter().find(|c| c.name == format!("pixel-{POPUP_PLANE}")).unwrap();
        assert!(popup.hashes.iter().all(|h| *h == ZERO), "an all-zero array is all zero markers");
        assert!(!cs.has(&ZERO));
        let colour = list.iter().find(|c| c.name == "pixel-1").unwrap();
        assert!(colour.hashes.iter().any(|h| *h != ZERO) && colour.hashes.iter().all(|h| *h == ZERO || cs.has(h)));
    }

    /// A missing chunk is refused and the board is left as it was; so is a name no
    /// store position has.
    #[test]
    fn a_bad_snapshot_leaves_the_board_alone() {
        let d = dir("bad");
        let cs = ChunkStore::new(&*d);
        let mut a = drawn();
        let list = a.save_chunks(&cs).unwrap();
        let mut b = board();
        b.store.write(1, 1, 1, 77);
        let before = b.store.checksum();
        let gone = list.iter().find(|c| c.name == "pixel-1").unwrap().hashes.iter().find(|h| **h != ZERO).copied().unwrap();
        std::fs::remove_file(cs.path_for(&gone)).unwrap();
        assert!(b.load_chunks(&cs, &list).is_err());
        assert_eq!(b.store.checksum(), before);
        let odd = [KonaChunk { name: "pixel-9999".into(), hashes: vec![] }];
        assert!(b.load_chunks(&cs, &odd).unwrap_err().contains("no word position"));
        assert!(b.load_chunks(&cs, &[KonaChunk { name: "nonsense".into(), hashes: vec![] }]).is_err());
    }

    /// The cursor memory comes back with its registers: the glyph the host sees is the same.
    #[test]
    fn the_cursor_memory_is_restored() {
        let d = dir("cursor");
        let cs = ChunkStore::new(&*d);
        let mut a = board();
        {
            let mut t = a.tables.lock();
            let mut glyph = vec![0u32; 260];
            glyph[0] = 0x8000_0000;
            t.load_cursor(260, &glyph);
            t.indirect(0x101, 260);
            t.indirect(0x102, 34 | 33 << 13);
            t.indirect(0x103, 2);
        }
        let list = a.save_chunks(&cs).unwrap();
        let mut b = board();
        {
            // The registers are the small state, saved elsewhere; set as a restore would.
            let mut t = b.tables.lock();
            t.indirect(0x101, 260);
            t.indirect(0x102, 34 | 33 << 13);
            t.indirect(0x103, 2);
        }
        b.load_chunks(&cs, &list).unwrap();
        assert_eq!(b.tables.lock().snapshot().hw_cursor(), a.tables.lock().snapshot().hw_cursor());
        assert!(b.tables.lock().snapshot().hw_cursor().is_some());
    }

    #[test]
    fn texture_memory_is_restored_by_page() {
        let d = dir("texture");
        let cs = ChunkStore::new(&*d);
        let mut a = board();
        let t = a.store.texture();
        assert!(t.ex().set(2, 0, 0xff01_0203) && t.ex().set(2, 69_999, 0xff0a_0b0c) && t.ex().set(5, 17, 7));
        let list = a.save_chunks(&cs).unwrap();
        let named = |n: &str| list.iter().find(|c| c.name == n).map(|c| c.hashes.iter().filter(|h| **h != ZERO).count());
        assert_eq!((named("texture-g2-5"), named("texture-g5-1")), (Some(2), Some(1)), "a chunk to a held page, a marker to the rest");
        let mut b = board();
        b.load_chunks(&cs, &list).unwrap();
        let t = b.store.texture();
        assert_eq!((t.used(), t.get(2, 0), t.get(2, 69_999), t.get(5, 17), t.resident_pages()), (vec![(2, 5), (5, 1)], 0xff01_0203, 0xff0a_0b0c, 7, 3));
        assert_eq!(b.save_chunks(&cs).unwrap(), list);
    }

    /// A plane that is not one, is repeated or is longer than a bank, a table of the wrong length, pages past the
    /// ceiling, and the older layouts' chunks, are refused.
    #[test]
    fn texture_planes_from_a_snapshot_are_checked() {
        let d = dir("texture-planes");
        let cs = ChunkStore::new(&*d);
        let mut a = board();
        assert!(a.store.texture().ex().set(1, 0, 1));
        let list = a.save_chunks(&cs).unwrap();
        let h = put(&cs, &[0; PAGE_WORDS]).unwrap();
        let with = |name: &str, n: usize| -> Vec<KonaChunk> { list.iter().cloned().chain([KonaChunk { name: name.into(), hashes: h.iter().cycle().take(n).copied().collect() }]).collect() };
        let past = format!("texture-g3-{}", a.store.texture().plane_pages() + 1);
        for (name, n) in [("texture-g1-1", 1), (&format!("texture-g{PLANES}-1"), 1), ("texture-g3-2", 1), (past.as_str(), a.store.texture().plane_pages() + 1), ("texture-g9-999999999999", 1), ("texture-p3-8", 1), ("texture-8", 1)] {
            assert!(board().load_chunks(&cs, &with(name, n)).is_err(), "{name}");
        }
        assert!(board().load_chunks(&cs, &with("texture-g3-1", 1)).is_ok());
    }

    /// The manifest names every chunk the board saved, so the garbage collector keeps
    /// them, and not the zero marker.
    #[test]
    fn the_manifest_keeps_the_boards_chunks_alive() {
        use crate::snapshot::ChunksManifest;
        let d = dir("gc");
        let cs = ChunkStore::new(&*d);
        let mut a = drawn();
        let list = a.save_chunks(&cs).unwrap();
        let orphan = cs.put(b"nothing refers to this").unwrap();
        let m = ChunksManifest { kona_chunks: list.clone(), ..Default::default() };
        let live: std::collections::HashSet<_> = m.referenced_hashes().copied().collect();
        assert!(!live.contains(&ZERO) && live.len() >= 2);
        cs.gc(&live).unwrap();
        assert!(list.iter().flat_map(|c| &c.hashes).all(|h| *h == ZERO || cs.has(h)), "the board's chunks survive");
        assert!(!cs.has(&orphan), "an unreferenced one does not");
        // And the manifest round-trips through postcard as the registry reads it.
        let bytes = postcard::to_allocvec(&m).unwrap();
        assert_eq!(postcard::from_bytes::<ChunksManifest>(&bytes).unwrap().kona_chunks, list);
    }
}
