//! The display option board (a GVO) as a device of the shell's: the display-bus accesses the firmware makes for it
//! when the X server probes it, loads its logic and sets its data format, the channel that exists once the
//! probe finds it, and the window of the managed area that channel shows.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use parking_lot::Mutex as PMutex;
use sgi_engine::{BoardEngine, Chunk, HostMem, MemFault, Source, Swap, VecSource, Write};
use sgi_kona::dev::kona::back::display::option::DisplayBusSlave;
use sgi_kona::dev::kona::defs::*;
use sgi_kona::dev::kona::engine::{KonaEngine, Options};
use sgi_kona::dev::kona::ident::KonaConfig;

#[derive(Default)]
struct Ram(PMutex<Vec<u8>>);

impl HostMem for Ram {
    fn ram_read(&self, addr: u64, buf: &mut [u8], _: Swap) -> Result<(), MemFault> {
        let m = self.0.lock();
        buf.iter_mut().enumerate().for_each(|(i, b)| *b = m.get(addr as usize + i).copied().unwrap_or(0));
        Ok(())
    }

    fn ram_write(&self, addr: u64, data: &[u8], _: Swap) -> Result<(), MemFault> {
        let mut m = self.0.lock();
        if m.len() < addr as usize + data.len() {
            m.resize(addr as usize + data.len(), 0);
        }
        m[addr as usize..addr as usize + data.len()].copy_from_slice(data);
        Ok(())
    }
}

type Engine = KonaEngine<Ram>;

/// A board that stores every register, answers the load data's bit 7 at once and logs every store.
#[derive(Clone, Default)]
struct Board {
    regs: Arc<Mutex<BTreeMap<u32, u32>>>,
    log: Arc<Mutex<Vec<(u32, u32)>>>,
}

impl DisplayBusSlave for Board {
    fn read(&mut self, addr: u32) -> Option<u32> {
        let v = self.regs.lock().unwrap().get(&addr).copied().unwrap_or(0);
        Some(if addr == 0x8a02 { v | 0x80 } else { v })
    }

    fn write(&mut self, addr: u32, data: u32) -> bool {
        self.log.lock().unwrap().push((addr, data));
        self.regs.lock().unwrap().insert(addr, data);
        true
    }
}

const A: Source = Source { id: 1, charged: true };

fn engine(display: &str) -> Engine {
    let cfg = KonaConfig::ir2(1).unwrap().with_option_slave().with_display(display, 2).unwrap().with_pool(0, 32);
    Engine::new(cfg, Ram::default(), Options::default(), None).expect("engine")
}

/// The video command `command` with `data`, as the X server frames it on the command page.
fn video(e: &mut Engine, command: u32, data: &[u32]) {
    let sum = data.iter().fold(command, |s, w| s.wrapping_add(*w));
    for w in [0x1a6, command].into_iter().chain(data.iter().copied()).chain([sum]) {
        e.set_source(A);
        assert_eq!(e.write(COMMAND_PAGE, 32, w as u64), Write::Done);
    }
    e.run(usize::MAX);
}

/// A format load for `channel`: `lines` by `pixels` active, 30 frames a second.
fn load_format(e: &mut Engine, channel: u32, pixels: u32, lines: u32) {
    let mut format = [0u32; 383];
    (format[0], format[1], format[19], format[20], format[21], format[22], format[23], format[24]) = (channel, 1, 30 << 16, 1, 525, 858, lines, pixels);
    video(e, VIDEO_LOAD_FORMAT, &format);
}

fn the_probed_pipe() -> (Engine, Board) {
    let mut e = engine("dg5-2-gvo");
    let board = Board::default();
    e.set_display_bus(Some(Box::new(board.clone())));
    video(&mut e, 0x53, &[0]);
    (e, board)
}

#[test]
fn the_option_channel_exists_once_the_probe_finds_the_board_and_a_plain_pipe_has_none() {
    let mut e = engine("dg5-2-gvo");
    assert_eq!(e.channel_window_of(0xc6), None, "no device, no probe");
    let board = Board::default();
    e.set_display_bus(Some(Box::new(board.clone())));
    video(&mut e, 0x53, &[0]);
    assert_eq!(board.log.lock().unwrap()[..2], [(0xc831, 0xc6 + 0x3e8), (0xc931, 0xc7 + 0x3e8)], "the VOC probe");
    load_format(&mut e, 0xc6, 720, 486);
    video(&mut e, VIDEO_RESIZE, &[0xc6, 0, 485, 720, 486, 1, 1, 0]);
    // The window is the input rectangle: x 0, the top row 485 from the bottom of the 1024 rows, 720 by 486.
    assert_eq!(e.channel_window_of(0xc6), Some((0, 1024 - 1 - 485, 720, 486)));
    assert_eq!(e.channel_window_of(2), e.channel_window_of(0xc6), "named by its index too");
    assert_eq!(e.channel_window_of(0), Some((0, 0, 1280, 1024)), "channel 0 keeps its desktop format");
    assert_eq!(e.head_timing(0).map(|t| t.lines), None, "no format loaded on a display channel: the shell's default");

    let mut plain = Engine::new(KonaConfig::ir2(1).unwrap().with_pool(0, 32), Ram::default(), Options::default(), None).unwrap();
    plain.set_display_bus(Some(Box::new(Board::default())));
    video(&mut plain, 0x53, &[0]);
    load_format(&mut plain, 0xc6, 720, 486);
    assert_eq!(plain.channel_window_of(0xc6), None, "a pipe without the option has no such channel");
}

#[test]
fn the_gvo_format_no_longer_replaces_channel_0() {
    let (mut e, _) = the_probed_pipe();
    load_format(&mut e, 0, 1280, 1024);
    let before = e.head_timing(0).expect("channel 0 runs a format");
    load_format(&mut e, 0xc6, 720, 486);
    assert_eq!(e.head_timing(0), Some(before), "channel 0 is as it was");
}

#[test]
fn the_logic_download_and_the_data_format_reach_the_board() {
    let (mut e, board) = the_probed_pipe();
    board.log.lock().unwrap().clear();
    video(&mut e, 0x52, &[0, u32::MAX, 0, 0, 0, 0]);
    video(&mut e, 0x52, &[0, 3, 0xaa55_c300, 0, 0, 0]);
    assert_eq!(*board.log.lock().unwrap(), vec![(0x8900, 1), (0x8a02, 0xaa), (0x8a02, 0x55), (0x8a02, 0xc3)]);
    video(&mut e, 0x58, &[0xc6, 3, 0]);
    let regs = board.regs.lock().unwrap();
    assert_eq!((regs[&0x8400], regs[&0xc006], regs[&0xc214], regs[&0xc215], regs[&0xc216]), (0x800, 0x484, 0x31, 0xa7, 0));
}

#[test]
fn requests_1_and_2_read_and_write_the_boards_registers() {
    let (mut e, board) = the_probed_pipe();
    video(&mut e, 0x01, &[0xc104, 0x5a]);
    assert_eq!(board.regs.lock().unwrap()[&0xc104], 0x5a);
}

#[test]
fn the_channel_scanout_is_the_input_rectangle_of_the_frame() {
    let (mut e, _) = the_probed_pipe();
    load_format(&mut e, 0xc6, 4, 2);
    // Two rows whose top is the 11th from the bottom of the managed area, 4 wide, at x 8.
    video(&mut e, VIDEO_RESIZE, &[0xc6, 8, 10, 4, 2, 1, 1, 0]);
    // Colour 1 is a pure red; a box of it over the whole managed area.
    for (selector, args) in [(0x3, &[1, 0xff << 2, 0, 0][..]), (0x2, &[1]), (0x5, &[0, 0, 1279, 1023])] {
        for w in [&[1, selector][..], args].concat() {
            e.set_source(A);
            assert_eq!(e.write(COMMAND_PAGE, 32, w as u64), Write::Done);
        }
        e.run(usize::MAX);
    }
    let (w, h, pixels) = e.channel_scanout_of(0xc6).expect("the window");
    assert_eq!((w, h, pixels.len()), (4, 2, 8));
    assert!(pixels.iter().all(|&p| p == 0xff_00_00), "the box is red: {pixels:x?}");
    let (fw, fh, frame) = e.compose();
    assert_eq!((fw, fh), (1280, 1024));
    let mut whole = Vec::new();
    BoardEngine::scanout(&mut e, 0, &mut whole).unwrap();
    assert_eq!(whole.len(), frame.len());
    assert_ne!(pixels, vec![0; 8]);
    assert_eq!(e.channel_scanout_of(5), None, "no such channel");
}

#[test]
fn the_probe_survives_a_save_and_a_load_and_a_reset_forgets_it() {
    let (mut e, board) = the_probed_pipe();
    load_format(&mut e, 0xc6, 720, 486);
    video(&mut e, VIDEO_RESIZE, &[0xc6, 0, 485, 720, 486, 1, 1, 0]);
    e.quiesce(u64::MAX).unwrap();
    let mut chunks: Vec<Chunk> = Vec::new();
    e.save(&mut chunks).unwrap();
    e.resume();

    let mut f = engine("dg5-2-gvo");
    f.set_display_bus(Some(Box::new(board)));
    f.quiesce(u64::MAX).unwrap();
    f.load(&mut VecSource::new(chunks)).unwrap();
    f.resume();
    assert!(f.channel_window_of(0xc6).is_some_and(|w| w.2 == 720), "the loaded pipe has the channel and its format");

    f.reset().unwrap();
    assert_eq!(f.channel_window_of(0xc6), None, "power-on: not probed yet");
}
