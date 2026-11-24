//! The GR2 engine's saved state as named chunks (`sgi_engine::Chunk`): a `meta` chunk first, one `board`
//! chunk of everything small, and the large stores a piece at a time with the all-zero pieces left out.
//! Everything is little-endian. The HQ2 interpreter's own state is not in it (see `engine.rs`).

use std::sync::atomic::Ordering;

use sgi_engine::{Chunk, Fault, Refused, StateSink, StateSource};

use super::board::{Gr2, GEDMA_OUT_WORDS};
use super::re3::{FB_H, FB_W};
use super::vc1::SRAM_SIZE;
use super::xmap5::CLUT_SIZE;
use super::ge7::{GeMicroword, GE_MAX, GE_UCODE_WORDS, GE_WINDOW_WORDS};

const PIECE_WORDS: usize = 4096;
const PIECE_BYTES: usize = 16 * 1024;

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.u64(v as u64);
    }
    fn words(&mut self, v: &[u32]) {
        v.iter().for_each(|&w| self.u32(w));
    }
}

struct R<'a>(&'a [u8]);

impl R<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let (head, rest) = self.0.split_first_chunk::<N>().ok_or("the board chunk ends early")?;
        self.0 = rest;
        Ok(*head)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take::<1>()?[0])
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take()?))
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take()?))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take()?))
    }
    fn i64(&mut self) -> Result<i64, String> {
        Ok(self.u64()? as i64)
    }
    fn words(&mut self, out: &mut [u32]) -> Result<(), String> {
        for w in out {
            *w = self.u32()?;
        }
        Ok(())
    }
    fn bytes(&mut self, out: &mut [u8]) -> Result<(), String> {
        let (head, rest) = self.0.split_at_checked(out.len()).ok_or("the board chunk ends early")?;
        out.copy_from_slice(head);
        self.0 = rest;
        Ok(())
    }
}

fn put(sink: &mut dyn StateSink, name: &str, index: u64, bytes: Vec<u8>) -> Result<(), Fault> {
    sink.put(Chunk { name: name.into(), index, bytes })
}

/// A store of 32-bit words a piece of `PIECE_WORDS` a chunk, the zero pieces left out.
fn put_words(sink: &mut dyn StateSink, name: &str, base: u64, words: &[u32]) -> Result<(), Fault> {
    for (i, c) in words.chunks(PIECE_WORDS).enumerate() {
        if c.iter().any(|&w| w != 0) {
            put(sink, name, base + i as u64, c.iter().flat_map(|w| w.to_le_bytes()).collect())?;
        }
    }
    Ok(())
}

fn put_bytes(sink: &mut dyn StateSink, name: &str, bytes: &[u8]) -> Result<(), Fault> {
    for (i, c) in bytes.chunks(PIECE_BYTES).enumerate() {
        if c.iter().any(|&b| b != 0) {
            put(sink, name, i as u64, c.to_vec())?;
        }
    }
    Ok(())
}

fn meta(gr: &Gr2) -> Vec<u8> {
    let p = gr.probe();
    let mut w = W(Vec::new());
    w.u32(super::engine::GR2_STATE_VERSION);
    w.u32(p.ges);
    p.ports.iter().for_each(|&v| w.u32(v));
    w.u32(p.hq_rev);
    w.u32(p.aux_window as u32); // None 0, Legacy 1, Bytes 2
    w.u32(FB_W as u32);
    w.u32(FB_H as u32);
    w.0
}

/// The board's registers and small tables as one blob, in the order `load_board` reads them.
fn board(gr: &Gr2) -> Vec<u8> {
    let r = gr.regs();
    let mut w = W(Vec::new());
    let h = &r.hq;
    w.words(&h.attrjmp);
    for v in [h.numge, h.dmasync, h.fifo_full_timeout, h.fifo_empty_timeout, h.fifo_full, h.fifo_empty, h.loaducode, h.gepc, h.intr, h.refresh, h.running] {
        w.u32(v);
    }
    h.fin.iter().for_each(|f| w.u32(f.load(Ordering::Acquire)));
    w.u32(r.ge.count);
    for win in &r.ge.win {
        w.words(win);
    }
    w.words(&r.bdvers_w);
    w.u32(r.ab1);
    r.aux.iter().for_each(|&b| w.u8(b));
    w.u64(r.pll_bits);
    w.u32(r.pll_count);
    r.pll_last.iter().for_each(|&b| w.u8(b));
    w.u32(r.pll_programs);
    let v = &r.vc1;
    w.u16(v.addr);
    w.u8(v.sysctl);
    w.u8(v.cmd1);
    v.regs.iter().for_each(|&b| w.u8(b));
    for d in &r.dac {
        for b in [d.addr, d.readmask, d.blinkmask, d.cmd, d.test] {
            w.u8(b);
        }
        d.palette.iter().for_each(|&b| w.u8(b));
        d.overlay.iter().for_each(|&b| w.u8(b));
    }
    for x in &r.xmap {
        w.u16(x.addr);
        w.u8(x.clut_phase);
        w.u8(x.fifostatus_w);
        x.misc.iter().for_each(|&b| w.u8(b));
        w.words(&x.mode);
        x.clut_rg.iter().for_each(|&b| w.u8(b));
    }
    // SAFETY: the board is parked, so the RE3 thread is not running.
    let c = unsafe { &(*gr.re3.get()).ctx };
    w.words(&c.reg);
    for v in [c.x, c.y, c.z, c.r, c.g, c.b] {
        w.i64(v);
    }
    for v in [c.numpix, c.stream, c.pixfmt, c.zctl] {
        w.u32(v);
    }
    w.u64(c.stencil);
    w.u64(c.zfill_rect);
    w.u32(c.blend);
    w.i64(c.a);
    w.i64(c.da);
    // The read port's unread words, oldest first.
    let (head, tail) = (gr.gedma_out_head.load(Ordering::Acquire), gr.gedma_out_tail.load(Ordering::Acquire));
    let n = head.wrapping_sub(tail) as usize;
    w.u32(n as u32);
    for i in 0..n {
        w.u32(gr.gedma_out[tail.wrapping_add(i as u32) as usize & (GEDMA_OUT_WORDS - 1)].load(Ordering::Relaxed));
    }
    w.0
}

fn load_board(gr: &Gr2, bytes: &[u8]) -> Result<(), String> {
    let r = gr.regs();
    let mut s = R(bytes);
    let h = &mut r.hq;
    s.words(&mut h.attrjmp)?;
    for v in [&mut h.numge, &mut h.dmasync, &mut h.fifo_full_timeout, &mut h.fifo_empty_timeout, &mut h.fifo_full, &mut h.fifo_empty, &mut h.loaducode, &mut h.gepc, &mut h.intr, &mut h.refresh, &mut h.running] {
        *v = s.u32()?;
    }
    for f in &h.fin {
        f.store(s.u32()?, Ordering::Release);
    }
    r.ge.count = s.u32()?;
    for win in &mut r.ge.win {
        s.words(win)?;
    }
    s.words(&mut r.bdvers_w)?;
    r.ab1 = s.u32()?;
    s.bytes(&mut r.aux)?;
    r.pll_bits = s.u64()?;
    r.pll_count = s.u32()?;
    s.bytes(&mut r.pll_last)?;
    r.pll_programs = s.u32()?;
    let v = &mut r.vc1;
    v.addr = s.u16()?;
    v.sysctl = s.u8()?;
    v.cmd1 = s.u8()?;
    s.bytes(&mut v.regs)?;
    for d in &mut r.dac {
        (d.addr, d.readmask, d.blinkmask, d.cmd, d.test) = (s.u8()?, s.u8()?, s.u8()?, s.u8()?, s.u8()?);
        s.bytes(&mut d.palette)?;
        s.bytes(&mut d.overlay)?;
    }
    for x in &mut r.xmap {
        x.addr = s.u16()?;
        x.clut_phase = s.u8()?;
        x.fifostatus_w = s.u8()?;
        s.bytes(&mut x.misc)?;
        s.words(&mut x.mode)?;
        s.bytes(&mut x.clut_rg)?;
    }
    // SAFETY: the board has not started, so no thread touches the RE3.
    let re3 = unsafe { &mut *gr.re3.get() };
    let c = &mut re3.ctx;
    s.words(&mut c.reg)?;
    for v in [&mut c.x, &mut c.y, &mut c.z, &mut c.r, &mut c.g, &mut c.b] {
        *v = s.i64()?;
    }
    for v in [&mut c.numpix, &mut c.stream, &mut c.pixfmt, &mut c.zctl] {
        *v = s.u32()?;
    }
    c.stencil = s.u64()?;
    c.zfill_rect = s.u64()?;
    c.blend = s.u32()?;
    c.a = s.i64()?;
    c.da = s.i64()?;
    #[cfg(feature = "gr2-jit")]
    re3.jit.invalidate();
    let n = s.u32()? as usize;
    if n > GEDMA_OUT_WORDS {
        return Err(format!("{n} unread read-port words: the port holds {GEDMA_OUT_WORDS}"));
    }
    for i in 0..n {
        gr.gedma_out[i].store(s.u32()?, Ordering::Relaxed);
    }
    gr.gedma_out_head.store(n as u32, Ordering::Release);
    gr.gedma_out_tail.store(0, Ordering::Release);
    if !s.0.is_empty() {
        return Err(format!("{} bytes after the board chunk's end", s.0.len()));
    }
    Ok(())
}

/// The microword store as one run of words, five to a microword.
fn ge_words(gr: &Gr2) -> Vec<u32> {
    gr.regs().ge.ucode.iter().flat_map(|m| m.stage.into_iter().chain([m.hq])).collect()
}

pub fn save(gr: &Gr2, sink: &mut dyn StateSink) -> Result<(), Fault> {
    put(sink, "meta", 0, meta(gr))?;
    put(sink, "board", 0, board(gr))?;
    let r = gr.regs();
    put_words(sink, "shram", 0, &r.shram)?;
    put_words(sink, "hqucode", 0, &r.hq.ucode)?;
    put_words(sink, "geucode", 0, &ge_words(gr))?;
    put_bytes(sink, "vc1sram", &r.vc1.sram)?;
    for (i, x) in r.xmap.iter().enumerate() {
        put_words(sink, "clut", (i * (CLUT_SIZE / PIECE_WORDS)) as u64, &x.clut)?;
    }
    // SAFETY: the board is parked, so the RE3 thread is not running.
    let re3 = unsafe { &*gr.re3.get() };
    put_words(sink, "vram", 0, &re3.vram)?;
    put_words(sink, "zbuf", 0, &re3.zbuf)
}

pub fn load(gr: &Gr2, src: &mut dyn StateSource) -> Result<(), Refused> {
    let bad = |m: String| Refused::Mismatch(m);
    let mut seen_meta = false;
    let mut seen_board = false;
    let mut ge = vec![0u32; GE_UCODE_WORDS * 5];
    while let Some(c) = src.next().map_err(|e| bad(e.0))? {
        if !seen_meta {
            if c.name != "meta" || c.bytes != meta(gr) {
                return Err(bad(format!("the state's first chunk is {}, which is not this board's meta", c.name)));
            }
            seen_meta = true;
            continue;
        }
        let r = gr.regs();
        // SAFETY: the board has not started, so no thread touches the RE3.
        let re3 = unsafe { &mut *gr.re3.get() };
        match c.name.as_str() {
            "board" if !seen_board => {
                load_board(gr, &c.bytes).map_err(bad)?;
                seen_board = true;
            }
            "shram" => piece_words(&mut r.shram, &c).map_err(bad)?,
            "hqucode" => piece_words(&mut r.hq.ucode, &c).map_err(bad)?,
            "geucode" => piece_words(&mut ge, &c).map_err(bad)?,
            "vc1sram" => piece_bytes(&mut r.vc1.sram, &c).map_err(bad)?,
            "clut" => {
                let per = CLUT_SIZE / PIECE_WORDS;
                let (x, piece) = ((c.index as usize) / per, (c.index as usize) % per);
                let Some(xm) = r.xmap.get_mut(x) else { return Err(bad(format!("colour table piece {}", c.index))) };
                piece_words(&mut xm.clut, &Chunk { index: piece as u64, ..c }).map_err(bad)?;
            }
            "vram" => piece_words(&mut re3.vram, &c).map_err(bad)?,
            "zbuf" => piece_words(&mut re3.zbuf, &c).map_err(bad)?,
            other => return Err(bad(format!("unexpected state chunk {other}"))),
        }
    }
    if !seen_meta || !seen_board {
        return Err(bad("the state has no board chunk".into()));
    }
    for (m, w) in gr.regs().ge.ucode.iter_mut().zip(ge.chunks_exact(5)) {
        *m = GeMicroword { stage: [w[0], w[1], w[2], w[3]], hq: w[4] };
    }
    gr.mark_dirty();
    Ok(())
}

fn piece_words(dst: &mut [u32], c: &Chunk) -> Result<(), String> {
    let off = (c.index as usize).saturating_mul(PIECE_WORDS);
    let n = c.bytes.len() / 4;
    // A piece is a whole `PIECE_WORDS`, or the store's remainder: one cut short would load as zeros.
    if c.bytes.len() % 4 != 0 || off.saturating_add(n) > dst.len() || n != PIECE_WORDS.min(dst.len() - off.min(dst.len())) {
        return Err(format!("{} piece {} has {} bytes", c.name, c.index, c.bytes.len()));
    }
    for (d, b) in dst[off..off + n].iter_mut().zip(c.bytes.chunks_exact(4)) {
        *d = u32::from_le_bytes(b.try_into().unwrap());
    }
    Ok(())
}

fn piece_bytes(dst: &mut [u8], c: &Chunk) -> Result<(), String> {
    let off = (c.index as usize).saturating_mul(PIECE_BYTES);
    if off.saturating_add(c.bytes.len()) > dst.len() || c.bytes.len() != PIECE_BYTES.min(dst.len() - off.min(dst.len())) {
        return Err(format!("{} piece {} has {} bytes", c.name, c.index, c.bytes.len()));
    }
    dst[off..off + c.bytes.len()].copy_from_slice(&c.bytes);
    Ok(())
}
