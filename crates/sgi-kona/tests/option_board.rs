//! The display option board (a GVO) as a device of the shell's: the display-bus accesses the firmware makes for it
//! when the X server probes it, loads its logic and sets its data format, the channel that exists once the
//! probe finds it, and the window of the managed area that channel shows.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use parking_lot::Mutex as PMutex;
use sgi_engine::{BoardEngine, Chunk, HostMem, MemFault, Source, Swap, VecSource, Write};
use sgi_kona::dev::kona::back::display::option::DisplayBusSlave;
use sgi_kona::dev::kona::defs::*;
use sgi_kona::dev::kona::engine::{KonaEngine, Options, Scan};
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

#[test]
fn the_display_boards_channel_count_is_asked_not_derived() {
    for (display, channels) in [("dg5-2", 2), ("dg5-2-gvo", 2)] {
        assert_eq!(engine(display).display_channels(), channels, "{display}");
    }
    let deskside = Engine::new(KonaConfig::ir2(2).unwrap().with_pool(0, 32), Ram::default(), Options::default(), None).unwrap();
    assert_eq!(deskside.display_channels(), 8, "the deskside's DG5-8");
}

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

/// The X server loads the logic (request 82) before the firmware probe (requests 17 and 83): the download reaches the board
/// whether or not the probe has run, and the board's channel still waits for the probe.
#[test]
fn the_logic_download_reaches_the_board_before_the_probe() {
    let mut e = engine("dg5-2-gvo");
    let board = Board::default();
    e.set_display_bus(Some(Box::new(board.clone())));
    video(&mut e, 0x52, &[0, u32::MAX, 0, 0, 0, 0]);
    video(&mut e, 0x52, &[0, 2, 0x1234_0000, 0, 0, 0]);
    assert_eq!(*board.log.lock().unwrap(), vec![(0x8900, 1), (0x8a02, 0x12), (0x8a02, 0x34)]);
    assert_eq!(e.channel_window_of(0xc6), None, "no channel until the probe finds the board");
    video(&mut e, 0x53, &[0]);
    load_format(&mut e, 0xc6, 720, 486);
    video(&mut e, VIDEO_RESIZE, &[0xc6, 0, 485, 720, 486, 1, 1, 0]);
    assert!(e.channel_window_of(0xc6).is_some());
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

/// A format load for `channel` at `rate` frames a second.
fn load_format_at(e: &mut Engine, channel: u32, pixels: u32, lines: u32, rate: u32) {
    let mut format = [0u32; 383];
    (format[0], format[1], format[19], format[20], format[21], format[22], format[23], format[24]) = (channel, 1, rate << 16, 1, lines + 40, pixels + 100, lines, pixels);
    video(e, VIDEO_LOAD_FORMAT, &format);
}

#[test]
fn each_running_channel_is_a_head_with_its_own_size_timing_and_window() {
    let mut e = engine("dg5-2");
    assert_eq!((e.heads(), e.frame_size(0), e.frame_size(1)), (1, Some((1280, 1024)), None), "channel 0 alone: the whole managed area");
    assert_eq!(e.head_timing(0), None, "no format loaded");
    load_format_at(&mut e, 0, 1280, 1024, 60);
    // Channel 1 shows a 640 by 480 window whose top row is the 500th from the bottom, at x 16.
    load_format_at(&mut e, 1, 640, 480, 50);
    video(&mut e, VIDEO_RESIZE, &[1, 16, 499, 640, 480, 1, 1, 0]);
    assert_eq!(e.heads(), 2);
    assert_eq!((e.frame_size(0), e.frame_size(1), e.frame_size(2)), (Some((1280, 1024)), Some((640, 480)), None));
    let (t0, t1) = (e.head_timing(0).unwrap(), e.head_timing(1).unwrap());
    assert_eq!((t0.clock_hz, t0.lines, t1.clock_hz, t1.lines), (60 << 16, 1064, 50 << 16, 520), "each head has its channel's timing");
    assert_eq!(e.head_timing(2), None);
    let mut out = Vec::new();
    assert_eq!(BoardEngine::scanout(&mut e, 1, &mut out), Ok(Some((640, 480))));
    assert_eq!(out.len(), 640 * 480);
    assert_eq!(e.channel_window(1), Some((16, 1024 - 1 - 499, 640, 480)));
    assert_eq!(BoardEngine::scanout(&mut e, 2, &mut out), Ok(None));
    assert!(out.is_empty());
    assert!(e.frame_generation(1) > 0 && e.frame_generation(2) == 0);
    // Changing channel 1's format moves its timing's generation and not channel 0's.
    load_format_at(&mut e, 1, 640, 480, 25);
    assert_eq!((e.head_timing(0).unwrap().generation, e.head_timing(1).unwrap().generation > t1.generation), (t0.generation, true));
    video(&mut e, VIDEO_DISABLE_CHANNEL, &[1]);
    assert_eq!(e.heads(), 1, "a disabled channel is no head");
}

#[test]
fn the_gvo_channel_is_the_last_head_and_shows_its_window_at_one_to_one() {
    let (mut e, _) = the_probed_pipe();
    load_format_at(&mut e, 0, 1280, 1024, 60);
    load_format_at(&mut e, 0xc6, 720, 486, 30);
    // The combination file's slot 11 reads the top-left 720 by 486 of the managed area.
    video(&mut e, VIDEO_RESIZE, &[0xc6, 0, 1023, 720, 486, 1, 1, 0]);
    assert_eq!(e.heads(), 2);
    assert_eq!((e.frame_size(0), e.frame_size(1)), (Some((1280, 1024)), Some((720, 486))));
    assert_eq!(e.channel_window(2), Some((0, 0, 720, 486)));
    let (t0, t1) = (e.head_timing(0).unwrap(), e.head_timing(1).unwrap());
    assert_eq!((t0.clock_hz, t1.clock_hz), (60 << 16, 30 << 16), "channel 0 keeps the desktop's timing, the GVO has its own");
    let (mut desk, mut gvo) = (Vec::new(), Vec::new());
    BoardEngine::scanout(&mut e, 0, &mut desk).unwrap();
    BoardEngine::scanout(&mut e, 1, &mut gvo).unwrap();
    assert_eq!((desk.len(), gvo.len()), (1280 * 1024, 720 * 486));
    for row in [0usize, 100, 485] {
        assert_eq!(gvo[row * 720..(row + 1) * 720], desk[row * 1280..row * 1280 + 720], "row {row} of the window is the top-left of the desktop");
    }
    let (mut ten, mut ten0) = (Vec::new(), Vec::new());
    assert_eq!(BoardEngine::scanout10(&mut e, 1, &mut ten), Ok(Some((720, 486))));
    BoardEngine::scanout10(&mut e, 0, &mut ten0).unwrap();
    assert_eq!((ten.len(), ten0.len()), (720 * 486, 1280 * 1024));
    assert!(BoardEngine::lend_frame(&mut e, 1).unwrap().is_none(), "a window is not a lendable frame");
}

/// DISPREQ_COPY_ANOTHER_CHANNEL_OUTPUT: `channel` takes `source`'s pixels.
fn copy_channel(e: &mut Engine, channel: u32, source: u32) {
    video(e, VIDEO_VAMPIRE, &[channel, source, 0, 0, 0, 0, 0]);
}

/// A red point at (100, 50) in the managed area, drawn through the textport, so a head's picture is not all black.
fn draw_a_point(e: &mut Engine) {
    for (selector, args) in [(0x3, &[1, 0xff << 2, 0, 0][..]), (0x2, &[1]), (0x6, &[100, 50])] {
        for w in [&[1, selector][..], args].concat() {
            e.set_source(A);
            assert_eq!(e.write(COMMAND_PAGE, 32, w as u64), Write::Done);
            e.run(usize::MAX);
        }
    }
}

/// Channel 1 runs 640 by 480 at 50 Hz in its own window until it is made to copy channel 0 (the boot combination does
/// that; kona devices/dg/README.md, `SourceChannel`: the source's pixels are copied): then its head is channel 0's
/// whole picture at channel 0's timing, and it follows when the source's timing changes or the copy ends.
#[test]
fn a_dependent_channels_head_shows_its_sources_picture_and_timing() {
    let mut e = engine("dg5-2");
    draw_a_point(&mut e);
    load_format_at(&mut e, 0, 1280, 1024, 60);
    load_format_at(&mut e, 1, 640, 480, 50);
    video(&mut e, VIDEO_RESIZE, &[1, 16, 499, 640, 480, 1, 1, 0]);
    let own = e.head_timing(1).unwrap();
    assert_eq!((e.frame_size(1), own.clock_hz), (Some((640, 480)), 50 << 16));

    copy_channel(&mut e, 1, 0);
    assert_eq!((e.heads(), e.frame_size(1), e.channel_window(1)), (2, Some((1280, 1024)), Some((0, 0, 1280, 1024))));
    let (t0, t1) = (e.head_timing(0).unwrap(), e.head_timing(1).unwrap());
    assert_eq!((t1.clock_hz, t1.lines, t1.fields), (t0.clock_hz, t0.lines, t0.fields), "the source's timing");
    assert!(t1.generation > own.generation, "the head's timing changed");
    let (mut a, mut b) = (Vec::new(), Vec::new());
    BoardEngine::scanout(&mut e, 0, &mut a).unwrap();
    BoardEngine::scanout(&mut e, 1, &mut b).unwrap();
    assert!(a.iter().any(|&p| p != 0), "the point is in the picture");
    assert_eq!(a, b, "the source's picture");

    // The source's timing changes: the dependent follows.
    load_format_at(&mut e, 0, 1280, 1024, 30);
    let t1b = e.head_timing(1).unwrap();
    assert_eq!(t1b.clock_hz, 30 << 16);
    assert!(t1b.generation > t1.generation);

    // Naming itself ends the copy; so does a source that does not run.
    copy_channel(&mut e, 1, 1);
    assert_eq!((e.frame_size(1), e.head_timing(1).unwrap().clock_hz), (Some((640, 480)), 50 << 16));
    copy_channel(&mut e, 1, 0);
    video(&mut e, VIDEO_DISABLE_CHANNEL, &[0]);
    assert_eq!((e.heads(), e.frame_size(0)), (1, Some((640, 480))), "channel 0 is off: channel 1 shows its own");
}

/// The combination names the GVO channel as its own source by its physical id, 198: independent, so its window and
/// timing stay its own.
#[test]
fn the_gvo_naming_itself_as_its_source_stays_independent() {
    let (mut e, _) = the_probed_pipe();
    load_format_at(&mut e, 0, 1280, 1024, 60);
    load_format_at(&mut e, 0xc6, 720, 486, 30);
    video(&mut e, VIDEO_RESIZE, &[0xc6, 0, 1023, 720, 486, 1, 1, 0]);
    copy_channel(&mut e, 0xc6, 0xc6);
    assert_eq!((e.frame_size(1), e.head_timing(1).unwrap().clock_hz), (Some((720, 486)), 30 << 16));
    copy_channel(&mut e, 0xc6, 0);
    assert_eq!((e.frame_size(1), e.head_timing(1).unwrap().clock_hz), (Some((1280, 1024)), 60 << 16), "dependent on channel 0");
}

/// A blanked channel's head scans out black (the frame is not: the other channels still show it), whichever of
/// the picture's paths a shell takes; unblanked it shows the frame again.
#[test]
fn a_blanked_channels_head_scans_out_black_and_the_others_keep_the_picture() {
    let mut e = engine("dg5-2");
    draw_a_point(&mut e);
    load_format_at(&mut e, 0, 1280, 1024, 60);
    load_format_at(&mut e, 1, 1280, 1024, 60);
    video(&mut e, VIDEO_RESIZE, &[1, 0, 1023, 1280, 1024, 1, 1, 0]);
    let (mut a, mut b, mut wide) = (Vec::new(), Vec::new(), Vec::new());
    BoardEngine::scanout(&mut e, 0, &mut a).unwrap();
    BoardEngine::scanout(&mut e, 1, &mut b).unwrap();
    assert!(a.iter().any(|&p| p != 0) && a == b, "both heads show the point");

    video(&mut e, VIDEO_CHANNEL_BLANKING, &[1, 1]);
    assert_eq!(BoardEngine::scanout(&mut e, 1, &mut b), Ok(Some((1280, 1024))), "the head keeps its size");
    assert!(b.iter().all(|&p| p == 0), "channel 1 is black");
    assert_eq!(BoardEngine::scanout10(&mut e, 1, &mut wide), Ok(Some((1280, 1024))));
    assert!(wide.iter().all(|&p| p == 0));
    assert!(e.channel_scanout(1).unwrap().2.iter().all(|&p| p == 0), "and so is what a device takes from it");
    BoardEngine::scanout(&mut e, 0, &mut b).unwrap();
    assert_eq!(a, b, "channel 0 still shows the picture");
    assert!(e.channel_scanout(0).unwrap().2.iter().any(|&p| p != 0));
    assert!(BoardEngine::lend_frame(&mut e, 1).unwrap().is_none(), "no lent frame for a head that shows none of it");

    video(&mut e, VIDEO_CHANNEL_BLANKING, &[1, 0]);
    BoardEngine::scanout(&mut e, 1, &mut b).unwrap();
    assert_eq!(a, b, "unblanked");
}

/// Blanking the GVO channel zeroes its look-up table, 1024 words at 0xf000 on the display bus (kona devices/dg/
/// gvo-option.md and arm1_functions.md, `dgBlankChannel`, established); unblanking, another channel's blanking and
/// a board that has not been probed write nothing there.
#[test]
fn blanking_the_gvo_channel_zeroes_its_look_up_table() {
    let (mut e, board) = the_probed_pipe();
    load_format_at(&mut e, 0, 1280, 1024, 60);
    load_format_at(&mut e, 0xc6, 720, 486, 30);
    let lut = |board: &Board| board.log.lock().unwrap().iter().filter(|(a, _)| (0xf000..0xf400).contains(a)).copied().collect::<Vec<_>>();
    video(&mut e, VIDEO_CHANNEL_BLANKING, &[0, 1]);
    video(&mut e, VIDEO_CHANNEL_BLANKING, &[0xc6, 0]);
    assert!(lut(&board).is_empty(), "channel 0's blanking and the GVO's unblanking leave the table");
    video(&mut e, VIDEO_CHANNEL_BLANKING, &[0xc6, 1]);
    assert_eq!(lut(&board), (0..1024).map(|i| (0xf000 + i, 0)).collect::<Vec<_>>());
    board.log.lock().unwrap().clear();
    video(&mut e, VIDEO_CHANNEL_BLANKING, &[2, 1]);
    assert_eq!(lut(&board).len(), 1024, "named by its index as well");

    let mut plain = engine("dg5-2-gvo");
    let none = Board::default();
    plain.set_display_bus(Some(Box::new(none.clone())));
    video(&mut plain, VIDEO_CHANNEL_BLANKING, &[0xc6, 1]);
    assert!(lut(&none).is_empty(), "no probe, no channel");
}

/// The GVO head carries ten bits (kona gvo-option.md: ten of the gamma RAM's twelve; `0xc006` bits 10-9 form the two
/// low bits), a monitor head eight. Words are XB30 (red low); the top eight bits are the 8-bit picture's. The
/// firmware's own mode (true bits, after the data format) shows the pixels' own bits, a control word with the mode
/// bits clear zeroes the low two.
#[test]
fn the_gvo_head_is_ten_bits_formed_as_the_control_register_says_and_a_monitor_head_is_eight() {
    let (mut e, _) = the_probed_pipe();
    // Colour 1: the eight bits 0xff, 0x55 and 0x00, a point 1000 rows up from the bottom, inside the window.
    for (selector, args) in [(0x3, &[1, 0x3fc, 0x155, 1][..]), (0x2, &[1]), (0x6, &[100, 1000])] {
        for w in [&[1, selector][..], args].concat() {
            e.set_source(A);
            assert_eq!(e.write(COMMAND_PAGE, 32, w as u64), Write::Done);
            e.run(usize::MAX);
        }
    }
    load_format_at(&mut e, 0, 1280, 1024, 60);
    load_format_at(&mut e, 0xc6, 720, 486, 30);
    video(&mut e, VIDEO_RESIZE, &[0xc6, 0, 1023, 720, 486, 1, 1, 0]);
    video(&mut e, 0x58, &[0xc6, 1, 0]);
    assert_eq!((BoardEngine::head_bits(&e, 0), BoardEngine::head_bits(&e, 1)), (8, 10));
    let (mut eight, mut ten) = (Vec::new(), Vec::new());
    assert_eq!(BoardEngine::scanout(&mut e, 1, &mut eight), Ok(Some((720, 486))));
    assert_eq!(BoardEngine::scanout10(&mut e, 1, &mut ten), Ok(Some((720, 486))));
    let lit: Vec<usize> = (0..eight.len()).filter(|&i| eight[i] != 0).collect();
    assert_eq!(lit.len(), 1, "the point is inside the window");
    assert_eq!(ten[lit[0]], 0x3ff | 0x155 << 10, "the firmware's data format leaves 0xc006 in true-bits mode");
    let top = |c: u32| (c >> 2) & 0xff;
    for (&a, &t) in eight.iter().zip(&ten) {
        assert_eq!(a, top(t) << 16 | top(t >> 10) << 8 | top(t >> 20), "the top eight bits are scanout's");
    }
    // Mode 0 (bits 10-9 clear): the two low bits of every component are zero.
    video(&mut e, 0x01, &[0xc006, 0x084]);
    BoardEngine::scanout10(&mut e, 1, &mut ten).unwrap();
    assert_eq!(ten[lit[0]], 0x3fc | 0x154 << 10);
    assert!(ten.iter().all(|&t| t & 0xc030_0c03 == 0), "no low bits and no unused bits in any component");
    // A blanked GVO channel is black at ten bits too.
    video(&mut e, VIDEO_CHANNEL_BLANKING, &[0xc6, 1]);
    BoardEngine::scanout10(&mut e, 1, &mut ten).unwrap();
    assert!(ten.iter().all(|&t| t == 0));
}

#[test]
fn a_scan_source_gives_the_gvo_heads_ten_bits_as_scanout10_does_and_no_monitor_head() {
    let (mut e, board) = the_probed_pipe();
    for (selector, args) in [(0x3, &[1, 0x3fc, 0x155, 1][..]), (0x2, &[1]), (0x6, &[100, 1000])] {
        for w in [&[1, selector][..], args].concat() {
            e.set_source(A);
            assert_eq!(e.write(COMMAND_PAGE, 32, w as u64), Write::Done);
            e.run(usize::MAX);
        }
    }
    load_format_at(&mut e, 0, 1280, 1024, 60);
    load_format_at(&mut e, 0xc6, 720, 486, 30);
    video(&mut e, VIDEO_RESIZE, &[0xc6, 0, 1023, 720, 486, 1, 1, 0]);
    video(&mut e, 0x58, &[0xc6, 1, 0]);
    video(&mut e, 0x01, &[0xc006, 0x684]);
    let control = board.regs.lock().unwrap()[&0xc006];
    let mut ten = Vec::new();
    assert_eq!(BoardEngine::scanout10(&mut e, 1, &mut ten), Ok(Some((720, 486))));
    assert!(ten.iter().any(|&t| t != 0), "the point is in the window");
    let src = e.scan_source().expect("a source");
    let mut out = vec![0; 720 * 486];
    assert_eq!(src.wide(1, control, &mut out[..10]).unwrap(), Some(Scan::Short(720, 486)));
    assert_eq!(src.wide(1, control, &mut out).unwrap(), Some(Scan::Done(720, 486)));
    assert!(out == ten, "the source's words are scanout10's");
    assert_eq!(src.wide(0, control, &mut out).unwrap(), None);
}
