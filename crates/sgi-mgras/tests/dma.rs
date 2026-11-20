//! Host DMA through the pools' page tables: a round trip through GE11 ERAM
//! (host to board, then board to host) over a table of 4-byte entries, and
//! over one of 8-byte entries on another node, as the Origin 2000 and
//! Onyx2 kernels build them.

mod support;

use sgi_engine::BoardEngine;
use sgi_mgras::dev::mgras::engine::{Board, Config, Mode, MgrasEngine};
use support::*;

/// One command-FIFO packet: header (command, payload bytes), then the payload.
fn packet(cmd: u32, payload: &[u32]) -> Vec<u32> {
    let mut p = vec![cmd << 8 | 4 * payload.len() as u32];
    p.extend_from_slice(payload);
    p
}

/// A DMA register token (`0x800 + register`).
fn dma_reg(reg: u32, payload: &[u32]) -> Vec<u32> {
    packet(0x800 + reg, payload)
}

fn pattern(n: u32) -> Vec<u8> {
    (0..n).flat_map(|i| (0xA5A5_0000 + i).to_be_bytes()).collect()
}

#[test]
fn a_table_of_four_byte_entries_maps_the_pages() {
    let mem = Mem::default();
    // Pool 0's table at 0x4018 (the low bits are flags), frames 0x7000 and 0x9000.
    mem.put(0x4018, &[0, 0, 0, 7, 0, 0, 0, 9]);
    mem.put(0x7010, &pattern(8));
    let mut cfg = Config::new(Board::Solid, Mode::Inline);
    cfg.entry_bytes = 4;
    let mut e = MgrasEngine::new(cfg, mem.clone(), None, None).unwrap();
    run(&mut e, (0, 0x4019), 8, 0x10, 0x1020);
    assert_eq!(mem.get(0x9020, 32), pattern(8));
}

#[test]
fn a_table_of_eight_byte_entries_maps_the_pages_on_its_node() {
    let mem = Mem::default();
    // The table on node 2, entry 0 at 0x4018; the frames on node 3, in the
    // entries' node bits (27:20).
    let table = 2 << 32 | 0x4018;
    mem.put(table, &[0, 0, 0, 0, 0x00, 0x30, 0x00, 0x07, 0, 0, 0, 0, 0x00, 0x30, 0x00, 0x09]);
    mem.put(3 << 32 | 0x7010, &pattern(8));
    let mut cfg = Config::new(Board::Solid, Mode::Inline);
    cfg.entry_bytes = 8;
    let mut e = MgrasEngine::new(cfg, mem.clone(), None, None).unwrap();
    run(&mut e, (2, 0x4019), 8, 0x10, 0x1020);
    assert_eq!(mem.get(3 << 32 | 0x9020, 32), pattern(8));
    // Nothing leaked to the same offsets on node 0.
    assert_eq!(mem.get(0x9020, 32), vec![0; 32]);
}

/// The same table read as 4-byte entries goes nowhere useful: the 8-byte
/// layout is not guessed from the data.
#[test]
fn an_eight_byte_table_read_as_four_byte_entries_moves_the_wrong_data() {
    let mem = Mem::default();
    let table = 0x4018u64;
    mem.put(table, &[0, 0, 0, 0, 0x00, 0x00, 0x00, 0x07, 0, 0, 0, 0, 0x00, 0x00, 0x00, 0x09]);
    mem.put(0x7010, &pattern(8));
    let mut cfg = Config::new(Board::Solid, Mode::Inline);
    cfg.entry_bytes = 4;
    let mut e = MgrasEngine::new(cfg, mem.clone(), None, None).unwrap();
    run(&mut e, (0, 0x4019), 8, 0x10, 0x1020);
    assert_ne!(mem.get(0x9020, 32), pattern(8));
}

/// Move `words` from host memory at logical byte `from` through pool 0's
/// table into ERAM, then out again at logical byte `to`.
fn run(e: &mut MgrasEngine<Mem>, table: (u32, u32), words: u32, from: u32, to: u32) {
    run_with_start(e, table, words, from, to, 1)
}

/// As `run`, the host to board DMA started with `start` (bit 0 runs it).
fn run_with_start(e: &mut MgrasEngine<Mem>, table: (u32, u32), words: u32, from: u32, to: u32, start: u32) {
    let mut fifo = Vec::new();
    fifo.extend(dma_reg(0x20, &[table.0, table.1]));
    for (reg, v) in [(0x04, 0), (0x05, 0), (0x07, 1), (0x08, 4 * words)] {
        fifo.extend(dma_reg(reg, &[v]));
    }
    // Host to ERAM word 0x40: the chunk, the DMA behind it, then the address.
    fifo.extend(packet(0x0FD, &[words]));
    fifo.extend(dma_reg(0x06, &[from]));
    fifo.extend(dma_reg(0x0B, &[start]));
    fifo.extend(packet(0x0FB, &[0x40, words]));
    fifo.extend(packet(0x0FA, &[]));
    // ERAM to host: the DMA armed first, then the read.
    fifo.extend(dma_reg(0x06, &[to]));
    fifo.extend(dma_reg(0x0B, &[1 | 8]));
    fifo.extend(packet(0x0FC, &[0x40, words]));
    assert_eq!(e.push_words(0, &fifo), fifo.len());
    drive(e);
}

/// The synthetic DMA recording kicks its transfer with a start word of 0.
#[test]
fn a_start_word_without_bit_0_runs_nothing() {
    let mem = Mem::default();
    mem.put(0x4018, &[0, 0, 0, 7, 0, 0, 0, 9]);
    mem.put(0x7010, &pattern(8));
    let mut e = MgrasEngine::new(Config::new(Board::Solid, Mode::Inline), mem.clone(), None, None).unwrap();
    run_with_start(&mut e, (0, 0x4019), 8, 0x10, 0x1020, 0);
    assert_eq!(mem.get(0x9020, 32), vec![0; 32]);
}

/// The table's control word names the XIO widget its memory is reached through (bits 19:16), and the engine puts it in
/// bits 51:48 of the address it hands the shell: the page-table fetch and the data both. Inline, where the transfer runs
/// on the caller's thread, the shell's bus is used; with a worker thread, the RAM path alone.
#[test]
fn the_tables_widget_is_in_the_address_and_inline_dma_uses_the_bus() {
    for (mode, bus) in [(Mode::Inline, true), (Mode::Threaded, false)] {
        let mem = Mem::default();
        let widget = 3u64 << 48;
        mem.put(widget | 0x4018, &[0, 0, 0, 7, 0, 0, 0, 9]);
        mem.put(widget | 0x7010, &pattern(8));
        // The same offsets without the widget hold other data, which must not be read or written.
        mem.put(0x7010, &[0xEE; 32]);
        let mut cfg = Config::new(Board::Solid, mode);
        cfg.entry_bytes = 4;
        let mut e = MgrasEngine::new(cfg, mem.clone(), None, None).unwrap();
        run(&mut e, (3 << 16, 0x4019), 8, 0x10, 0x1020);
        assert_eq!(mem.get(widget | 0x9020, 32), pattern(8), "{mode:?}");
        assert_eq!(mem.get(0x9020, 32), vec![0; 32], "{mode:?}: nothing on widget 0");
        let calls = mem.0.bus_calls.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(calls > 0, bus, "{mode:?}: {calls} bus calls");
    }
}
