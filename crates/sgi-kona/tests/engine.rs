//! The engine's interface from a shell's side, in inline mode. The credit values come from the
//! port of QEMU's rule (a charged store's credit goes to its sender when the pipe has taken its
//! words), not from a pipe.

use sgi_kona::dev::kona::back::textport::COLOUR_WORD;
use sgi_kona::dev::kona::defs::*;
use sgi_kona::dev::kona::engine::{FlashDrive, KonaEngine, Options};
use sgi_kona::dev::kona::flash::{FlashSink, FLASH_SIZE};
use sgi_kona::dev::kona::ident::KonaConfig;
use parking_lot::Mutex;
use sgi_engine::{shell, BoardEngine, Chunk, Event, HostMem, Idle, Interrupt, MemFault, Read, Refused, Source, StallOn, Swap, VecSource, Write};

#[derive(Default)]
struct Ram(Mutex<Vec<u8>>);

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

/// The widget the pipe sits at (`KonaConfig::widget`), the sender of its packets.
const SENDER: u8 = 0;

fn engine() -> Engine {
    Engine::new(KonaConfig::ir2(2).unwrap().with_pool(0, 32), Ram::default(), Options::default(), None).expect("engine")
}

fn store(e: &mut Engine, off: u32, val: u32, src: Source) {
    e.set_source(src);
    assert_eq!(e.write(off, 32, val as u64), Write::Done);
}

fn drain(e: &mut Engine) -> Vec<Event> {
    let mut v = Vec::new();
    e.drain_events(&mut v, usize::MAX);
    v
}

fn credits(events: &[Event]) -> Vec<(u32, u32)> {
    events.iter().filter_map(|e| if let Event::Credits { source, retired } = e { Some((*source, *retired)) } else { None }).collect()
}

fn packets(events: &[Event]) -> Vec<Interrupt> {
    events.iter().filter_map(|e| if let Event::Interrupt(p) = e { Some(*p) } else { None }).collect()
}

const A: Source = Source { id: 1, charged: true };
const B: Source = Source { id: 2, charged: true };
const NONE: Source = Source { id: 0, charged: false };

/// Textport words stored one at a time, each run at once; `senders` cycle.
fn textport(e: &mut Engine, words: &[u32], senders: &[Source]) {
    for (&w, &s) in words.iter().zip(senders.iter().cycle()) {
        store(e, COMMAND_PAGE, w, s);
        e.run(usize::MAX);
    }
}

/// The textport packet `1, 9, 0`: a sync word, a selector that takes one word, and that word.
fn textport_packet(e: &mut Engine, senders: [Source; 3]) {
    textport(e, &[1, 9, 0], &senders);
}

#[test]
fn a_packets_credits_return_with_its_last_word_to_that_words_sender() {
    let mut e = engine();
    textport_packet(&mut e, [A, B, A]);
    // The sync word is taken at once (1 credit to A); the selector opens the packet, which holds
    // its credit; the last word returns its own and the held one (2) to its sender, A. The shell
    // sees them as one event for A.
    assert_eq!(credits(&drain(&mut e)), [(1, 3)]);
    textport_packet(&mut e, [B, B, B]);
    assert_eq!(credits(&drain(&mut e)), [(2, 3)]);
}

#[test]
fn a_store_nobody_charged_earns_no_credit() {
    let mut e = engine();
    textport_packet(&mut e, [NONE, A, NONE]);
    // Only the charged middle word counts; the last word returns it, with no sender to name.
    assert_eq!(credits(&drain(&mut e)), [(0, 1)]);
}

#[test]
fn credits_wait_while_the_driver_has_parked_the_target() {
    let mut e = engine();
    store(&mut e, LOW_WINDOW_WRITE, 1, A);
    textport_packet(&mut e, [A, A, A]);
    assert_eq!(credits(&drain(&mut e)), [], "parked credits do not go back");
    // The three held credits and the parking store's own go to the store that clears the bit, and
    // that store's own credit follows.
    store(&mut e, LOW_WINDOW_WRITE, 0x22, B);
    assert_eq!(credits(&drain(&mut e)), [(2, 5)]);
}

/// A kernel command: its token by a slot store (the first word), the rest at the port.
fn kernel(e: &mut Engine, token: u32, words: &[u32]) {
    store(e, COMMAND_PAGE + (token << COMMAND_SLOT_SHIFT) + COMMAND_DATA_SLOT, words[0], A);
    for &w in &words[1..] {
        store(e, COMMAND_PAGE, w, A);
    }
    e.run(usize::MAX);
}

const SWAP: [u32; 3] = [0x3c0001, 13, 1];
/// The textport's colour 9, then a point at (7, 8): a draw.
const DRAW: [u32; 7] = [1, 2, 9, 1, 6, 7, 8];

#[test]
fn a_draw_behind_a_pending_swap_waits_for_the_frame() {
    let mut e = engine();
    kernel(&mut e, DISPLAY_MODE_BYTE, &SWAP);
    assert!(e.swap_waiting(), "the swap waits for a retrace");
    textport(&mut e, &DRAW, &[A]);
    assert_eq!(e.queued(), 7, "nothing behind the swap ran");
    assert_eq!(e.wait_idle(0, u64::MAX), Idle::Stalled(StallOn::Frame));
    assert_eq!(e.pixel(7, 8, COLOUR_WORD), 0, "the draw did not land");
    assert_eq!(e.advance(0).idle, Idle::Stalled(StallOn::Frame));
    assert!(matches!(e.quiesce(u64::MAX), Err(Refused::Midway(_))));

    e.frame(0);
    assert!(!e.swap_waiting());
    assert_eq!(e.wait_idle(0, u64::MAX), Idle::Yes);
    assert_eq!(e.pixel(7, 8, COLOUR_WORD), 9, "it landed once the swap had shown");
}

/// The shell's loop is what ends the stall, as the contract says.
#[test]
fn the_shell_loop_quiesces_an_engine_stalled_at_a_swap() {
    let mut e = engine();
    kernel(&mut e, DISPLAY_MODE_BYTE, &SWAP);
    textport(&mut e, &DRAW, &[A]);
    assert!(matches!(e.quiesce(u64::MAX), Err(Refused::Midway(_))));
    shell::quiesce(&mut e, &mut 0, 1_000_000, 8).expect("the loop gives the frame");
    assert_eq!((e.queued(), e.pixel(7, 8, COLOUR_WORD)), (0, 9));
}

#[test]
fn a_retrace_completes_at_the_frame_after_the_one_that_shows_the_swap() {
    let mut e = engine();
    // A destination, the vectors and the video cause enabled, as the driver programs them.
    store(&mut e, INT_DEST_LOWER, 0x1000, A);
    store(&mut e, INT_VECTORS, 0x20000 | 0x11, A);
    store(&mut e, INT_ENABLE, CAUSE_VIDEO, A);

    kernel(&mut e, DISPLAY_MODE_BYTE, &SWAP);
    kernel(&mut e, PUBLISH_RETRACE_SEQUENCE_NUMBER, &[0x1234]);
    assert_eq!(e.queued(), 1, "the request waits behind the swap");
    assert_eq!(e.read(RETRACE_TAG, 32), Read::Value(0), "the tag changes at the retrace, not at the request");
    drain(&mut e);

    // The frame that shows the swap starts the video interrupt, with no tag; the request behind
    // the swap is made after it and completes at the next frame.
    e.frame(0);
    assert!(!e.swap_waiting());
    e.run(usize::MAX);
    assert_eq!(e.read(RETRACE_TAG, 32), Read::Value(0), "the request was made after that retrace");
    assert_eq!(packets(&drain(&mut e)), [Interrupt { sender: SENDER, widget: 0, addr: 0x1000, vector: 0x11, level: true }], "the video interrupt, to the host");

    e.frame(0);
    assert_eq!(e.read(RETRACE_TAG, 32), Read::Value(0x1234));
}

/// A pending-bit packet adds nothing when an equal one waits unread; once the shell has it, the
/// next is another (the processor may have cleared the bit).
#[test]
fn a_packet_equal_to_one_still_queued_is_not_queued_again() {
    let mut e = engine();
    store(&mut e, INT_DEST_LOWER, 0x2000, A);
    store(&mut e, INT_VECTORS, 0x22, A);
    store(&mut e, INT_ENABLE, CAUSE_LOWATER, A);
    // Each packet that runs the stream empty raises the low-water cause.
    for _ in 0..3 {
        textport_packet(&mut e, [A, A, A]);
    }
    assert_eq!(packets(&drain(&mut e)), [Interrupt { sender: SENDER, widget: 0, addr: 0x2000, vector: 0x22, level: true }]);
    textport_packet(&mut e, [A, A, A]);
    assert_eq!(packets(&drain(&mut e)).len(), 1, "after a drain the next is another");
}

#[test]
fn a_packet_for_another_widget_goes_to_the_shell_with_that_widget() {
    let mut e = engine();
    store(&mut e, INT_DEST_UPPER, 3 << INT_DEST_WIDGET_SHIFT, A);
    store(&mut e, INT_DEST_LOWER, 0x40, A);
    store(&mut e, INT_VECTORS, 0x33, A);
    store(&mut e, INT_ENABLE, CAUSE_LOWATER, A);
    textport_packet(&mut e, [A, A, A]);
    assert_eq!(packets(&drain(&mut e)), [Interrupt { sender: SENDER, widget: 3, addr: 0x40, vector: 0x33, level: true }]);
    assert_eq!(e.irq_levels(), 0, "packets are the only signal");
}

#[test]
fn two_engines_share_nothing() {
    let (mut a, mut b) = (engine(), engine());
    store(&mut a, INT_VECTORS, 0x5a, A);
    a.run(usize::MAX);
    assert_eq!(b.read(INT_VECTORS, 32), Read::Value(0));
    assert_eq!(a.read(INT_VECTORS, 32), Read::Value(0x5a));
    textport_packet(&mut a, [A, A, A]);
    assert!(drain(&mut b).is_empty());
}

#[test]
fn a_read_of_a_register_the_stream_changes_waits_for_the_stream() {
    let mut e = engine();
    store(&mut e, COMMAND_PAGE, 1, A);
    assert_eq!(e.read(PORT_ADDR, 32), Read::Busy);
    assert!(e.access_waits(PORT_ADDR, 32, false));
    e.run(usize::MAX);
    assert_eq!(e.read(PORT_ADDR, 32), Read::Value(0));
}

#[test]
fn the_credit_register_is_the_words_waiting_shifted_left_one() {
    let mut e = engine();
    assert_eq!(e.read(CREDIT, 32), Read::Value(0));
    for w in [1, 9] {
        store(&mut e, COMMAND_PAGE, w, A);
    }
    assert_eq!(e.read(CREDIT, 32), Read::Value(4), "two words queued");
    e.run(usize::MAX);
    assert_eq!(e.read(CREDIT, 32), Read::Value(2), "the open command holds its selector");
    store(&mut e, COMMAND_PAGE, 0, A);
    e.run(usize::MAX);
    assert_eq!(e.read(CREDIT, 32), Read::Value(0));
}

#[test]
fn a_full_stream_answers_busy_and_takes_the_retry() {
    let opts = Options { queue: 4, ..Options::default() };
    let mut e = Engine::new(KonaConfig::ir2(1).unwrap().with_pool(0, 32), Ram::default(), opts, None).unwrap();
    e.set_source(A);
    for _ in 0..4 {
        assert_eq!(e.write(COMMAND_PAGE, 32, 1), Write::Done);
    }
    assert!(e.access_waits(COMMAND_PAGE, 32, true));
    assert_eq!(e.write(COMMAND_PAGE, 32, 1), Write::Busy);
    assert_eq!(e.run(2), 2);
    assert_eq!(e.write(COMMAND_PAGE, 32, 1), Write::Done);
}

#[test]
fn a_snapshot_taken_after_quiescing_loads_into_a_new_engine() {
    let mut e = engine();
    store(&mut e, INT_VECTORS, 0x77, A);
    e.run(usize::MAX);
    // Colour 9, then a box (2, 3) to (20, 21).
    textport(&mut e, &[1, 2, 9, 1, 5, 2, 3, 20, 21], &[A]);
    e.quiesce(u64::MAX).unwrap();
    let mut chunks: Vec<Chunk> = Vec::new();
    e.save(&mut chunks).expect("a snapshot between commands");
    assert_eq!(chunks[0].name, "meta", "meta first");
    assert!(chunks.iter().any(|c| c.name.starts_with("pixel-")), "the box is in the pixel pieces");
    e.resume();

    let mut f = engine();
    f.quiesce(u64::MAX).unwrap();
    f.load(&mut VecSource::new(chunks.clone())).expect("load");
    f.resume();
    assert_eq!(f.read(INT_VECTORS, 32), Read::Value(0x77));
    assert_eq!(f.compose().2, e.compose().2);
    let mut again: Vec<Chunk> = Vec::new();
    f.quiesce(u64::MAX).unwrap();
    f.save(&mut again).unwrap();
    assert_eq!(again, chunks, "save, load, save");
}

/// One store of a stream: the register and the word.
type Stores = Vec<(u32, u32)>;

fn plain(words: &[u32]) -> Stores {
    words.iter().map(|&w| (COMMAND_PAGE, w)).collect()
}

/// A kernel command: its token by a slot store with the first word, then the rest.
fn token(token: u32, words: &[u32]) -> Stores {
    let mut s = vec![(COMMAND_PAGE + (token << COMMAND_SLOT_SHIFT) + COMMAND_DATA_SLOT, words[0])];
    s.extend(plain(&words[1..]));
    s
}

fn feed(e: &mut Engine, stores: &[(u32, u32)]) {
    for &(off, val) in stores {
        store(e, off, val, A);
        e.run(usize::MAX);
    }
}

fn credit_totals(events: &[Event]) -> std::collections::BTreeMap<u32, u32> {
    let mut t = std::collections::BTreeMap::new();
    for (source, retired) in credits(events) {
        *t.entry(source).or_default() += retired;
    }
    t
}

/// A snapshot taken after every prefix of `stream` loads into a new engine that, given the rest,
/// ends where an engine that never stopped does: the same frame, the same saved state and the
/// same credits owed to the Hubs.
fn cut_everywhere(stream: &[(u32, u32)]) {
    let mut whole = engine();
    feed(&mut whole, stream);
    whole.quiesce(u64::MAX).unwrap();
    let mut want: Vec<Chunk> = Vec::new();
    whole.save(&mut want).unwrap();
    let owed = credit_totals(&drain(&mut whole));
    for k in 0..=stream.len() {
        let mut a = engine();
        feed(&mut a, &stream[..k]);
        a.quiesce(u64::MAX).unwrap();
        let mut chunks: Vec<Chunk> = Vec::new();
        a.save(&mut chunks).unwrap_or_else(|e| panic!("cut at {k}: {e:?}"));
        let mut before = credit_totals(&drain(&mut a));

        let mut b = engine();
        b.quiesce(u64::MAX).unwrap();
        b.load(&mut VecSource::new(chunks)).unwrap_or_else(|e| panic!("cut at {k}: {e:?}"));
        b.resume();
        feed(&mut b, &stream[k..]);
        b.quiesce(u64::MAX).unwrap();
        let mut got: Vec<Chunk> = Vec::new();
        b.save(&mut got).unwrap();
        assert_eq!(got, want, "cut at {k}: the state differs");
        assert_eq!(b.compose().2, whole.compose().2, "cut at {k}: the frame differs");
        for (hub, retired) in credit_totals(&drain(&mut b)) {
            *before.entry(hub).or_default() += retired;
        }
        assert_eq!(before, owed, "cut at {k}: the credits differ");
    }
}

#[test]
fn a_snapshot_mid_command_resumes_the_textport_commands() {
    // Colour 9 and a box, then a bitmap blit: its header, then its two payload words.
    let mut s = plain(&[1, 2, 9, 1, 5, 2, 3, 20, 21]);
    s.extend(plain(&[1, BITMAP_SELECTOR, 32, 2, 0, 0, 0, 0, 0xffff_0000, 0x0f0f_0f0f]));
    cut_everywhere(&s);
}

#[test]
fn a_snapshot_mid_command_resumes_a_kernel_command_and_the_words_behind_it() {
    // A colour map load takes its entries as a data run; a video request its data and a checksum.
    let mut s = token(LOAD_COLOUR_MAP_ENTRIES, &[0, 3, 0x111, 0x222, 0x333]);
    s.extend(token(DISPLAY_GENERATOR_REQUEST_CARRIER, &[VIDEO_TEMP_BLANK, 1, VIDEO_TEMP_BLANK + 1]));
    cut_everywhere(&s);
}

#[test]
fn a_snapshot_mid_command_resumes_a_flash_write_and_a_swap_in() {
    // Flash op 1, type 2 (the header), two words; then a swap-in of nothing: six words, a mode, a reason.
    let mut s = token(EEPROM_OPERATION, &[1, 2, 2, 0xaaaa_5555, 0x5555_aaaa]);
    s.extend(token(PIPE_MEMORY_SWAP_IN, &[0, 0, 0, 1, 1, 0]));
    s.extend(plain(&[DMA_ADDR_TAG, REASON_SWAP]));
    cut_everywhere(&s);
}

#[test]
fn a_snapshot_mid_command_resumes_a_list_call_and_an_open_primitive() {
    // 0x170 takes its ranges as a run: one range of two zero words in the list memory. The GL
    // grammar's early tokens open a primitive: begin, a colour, two vertices.
    let mut s = vec![(DMA_FIFO_PUSH, EXECUTE_LIST_BY_LIST_DMA_REPEATED), (DMA_FIFO_PUSH, 1)];
    s.extend(plain(&[0x2000, 2]));
    let f = |v: f32| v.to_bits();
    s.extend(plain(&[BEGIN_PRIMITIVE_INDEPENDENT_TRIANGLES, SET_CURRENT_COLOUR_3_COMPONENTS_SINGLE_FLOAT, f(1.0), f(0.0), f(0.0)]));
    for (x, y) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0)] {
        s.extend(plain(&[SUBMIT_VERTEX_2_COMPONENTS_SINGLE_FLOAT, f(x), f(y)]));
    }
    s.extend(plain(&[END_OPEN_PRIMITIVE]));
    cut_everywhere(&s);
}

#[test]
fn a_snapshot_that_does_not_fit_is_refused_and_is_not_a_device_fault() {
    let mut e = engine();
    e.quiesce(u64::MAX).unwrap();
    let mut chunks: Vec<Chunk> = Vec::new();
    e.save(&mut chunks).unwrap();
    for (what, bad) in [
        ("an empty stream", Vec::new()),
        ("no meta first", chunks.iter().skip(1).cloned().chain([Chunk { name: "state".into(), index: 0, bytes: vec![] }]).collect()),
        ("an unknown piece", chunks.iter().cloned().chain([Chunk { name: "nonsense".into(), index: 0, bytes: vec![] }]).collect()),
        ("a piece past the layout", chunks.iter().cloned().chain([Chunk { name: "pixel-9".into(), index: 0, bytes: vec![0; 4] }]).collect()),
    ] {
        assert!(matches!(e.load(&mut VecSource::new(bad)), Err(Refused::Mismatch(_))), "{what}");
    }
    assert!(e.fault().is_none());
    e.resume();
    e.reset().unwrap();
}

/// Programs the flash records go to a shared list, as a shell's drive callback would take them.
struct Drive(std::sync::Arc<Mutex<Vec<(usize, Vec<u8>)>>>);

impl FlashSink for Drive {
    fn write(&mut self, offset: usize, data: &[u8]) -> Result<(), String> {
        self.0.lock().push((offset, data.to_vec()));
        Ok(())
    }
}

fn on_drive(image: Vec<u8>) -> (Engine, std::sync::Arc<Mutex<Vec<(usize, Vec<u8>)>>>) {
    let log = std::sync::Arc::new(Mutex::new(Vec::new()));
    let opts = Options { flash: Some(FlashDrive { image, sink: Box::new(Drive(log.clone())) }), ..Options::default() };
    (Engine::new(KonaConfig::ir2(1).unwrap().with_pool(0, 32), Ram::default(), opts, None).expect("engine"), log)
}

#[test]
fn the_flash_starts_as_the_drive_and_what_the_pipe_programs_goes_back_to_it() {
    let mut image = vec![0u8; FLASH_SIZE];
    image[0x20000..0x20004].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    let (mut e, log) = on_drive(image.clone());
    // The EEPROM read of op 2 answers from the drive's contents: the reply is on the return
    // page, so read the flash through the indirect port instead (the PROM's way in).
    store(&mut e, PORT_ADDR, 0x800000 + 0x20000, A);
    e.run(usize::MAX);
    assert_eq!(e.read(PORT_DATA, 32), Read::Value(0xdead_beef));
    assert!(log.lock().is_empty(), "nothing is programmed by a read");
    // Op 1, type 2: two words into the header at 0x7ff00.
    feed(&mut e, &token(EEPROM_OPERATION, &[1, 2, 2, 0x1111_2222, 0x3333_4444]));
    let log2 = log.lock().clone();
    assert!(log2.iter().any(|(at, d)| *at == 0x7ff00 && d == &[0x11, 0x11, 0x22, 0x22]), "the first word reached the drive: {log2:?}");
    assert!(log2.iter().all(|(_, d)| d.len() == 4));

    // A snapshot load writes the whole image through, as the drive would otherwise miss it.
    e.quiesce(u64::MAX).unwrap();
    let mut chunks: Vec<Chunk> = Vec::new();
    e.save(&mut chunks).unwrap();
    let (mut f, flog) = on_drive(image);
    f.quiesce(u64::MAX).unwrap();
    f.load(&mut VecSource::new(chunks)).unwrap();
    let all = flog.lock().clone();
    assert_eq!(all.len(), 1);
    assert_eq!((all[0].0, all[0].1.len()), (0, FLASH_SIZE));
    assert_eq!(&all[0].1[0x7ff00..0x7ff04], &[0x11, 0x11, 0x22, 0x22]);
    assert!(Engine::new(KonaConfig::ir2(1).unwrap(), Ram::default(), Options { flash: Some(FlashDrive { image: vec![0; 12], sink: Box::new(Drive(Default::default())) }), ..Options::default() }, None).is_err(), "a drive of the wrong size");
}

#[test]
fn the_dma_widget_is_reported_at_start_and_at_each_change() {
    let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
    let tell = seen.clone();
    let opts = Options { dma_widget: Some(Box::new(move |w| tell.lock().push(w))), ..Options::default() };
    let cfg = KonaConfig::ir2(1).unwrap().with_pool(0, 32).with_host_widget(9).unwrap();
    let mut e = Engine::new(cfg, Ram::default(), opts, None).unwrap();
    assert_eq!(*seen.lock(), [9], "told at start");
    store(&mut e, WIDGET_CONTROL, 5 << 22, A);
    e.run(usize::MAX);
    assert_eq!(*seen.lock(), [9, 5]);
    store(&mut e, WIDGET_CONTROL, 5 << 22 | 1, A);
    e.run(usize::MAX);
    assert_eq!(*seen.lock(), [9, 5], "a write that leaves the widget is not a change");
    e.quiesce(u64::MAX).unwrap();
    let mut chunks: Vec<Chunk> = Vec::new();
    e.save(&mut chunks).unwrap();
    e.resume();
    store(&mut e, WIDGET_CONTROL, 7 << 22, A);
    e.run(usize::MAX);
    e.quiesce(u64::MAX).unwrap();
    e.load(&mut VecSource::new(chunks)).unwrap();
    assert_eq!(*seen.lock(), [9, 5, 7, 5], "a load brings the saved widget back");
    e.resume();
    e.reset().unwrap();
    assert_eq!(*seen.lock(), [9, 5, 7, 5, 9], "and a reset the configured one");
}

/// With a GPU, the frame is composed there and lent in place: reading it back gives what the CPU
/// compose gives. Without one, the call says there is nothing to lend.
#[test]
fn a_gpu_frame_is_lent_in_place() {
    use sgi_kona::wgpu::{Gpu, GpuOptions};
    let mut cpu = engine();
    assert!(cpu.lend_frame().unwrap().is_none(), "no GPU scan-out on a CPU pipe");
    let gpu = match Gpu::new(&GpuOptions::default(), Box::new(|m| panic!("GPU failed: {m}"))) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("skipped: {e}");
            return;
        }
    };
    let opts = Options { gpu: Some(gpu.clone()), ..Options::default() };
    let mut e = Engine::new(KonaConfig::ir2(1).unwrap().with_pool(0, 32), Ram::default(), opts, None).expect("engine");
    // Colour 1 is red; a point at (100, 50).
    for (selector, args) in [(0x3, &[1, 0xff << 2, 0, 0][..]), (0x2, &[1]), (0x6, &[100, 50])] {
        textport(&mut e, &[&[1, selector][..], args].concat(), &[A]);
    }
    let lent = e.lend_frame().unwrap().expect("a GPU scan-out");
    let (w, h, want) = e.compose();
    assert_eq!((lent.width, lent.height), (w, h));
    let mut got = vec![0u32; (w * h) as usize];
    gpu.read(&lent.buffer, &[(0, got.len() as u64 * 4)], bytemuck::cast_slice_mut(&mut got)).unwrap();
    assert_eq!(got, want);
    assert!(want.iter().any(|&p| p != 0), "the point is in the frame");
}

/// The trait's scan-out: one head, the picture as 0x00RRGGBB, nothing lent without a GPU unit.
#[test]
fn the_pipe_has_one_head_whose_picture_is_copied_as_rgb_and_not_lent_without_a_gpu() {
    let mut e = engine();
    assert_eq!(e.heads(), 1);
    let mut px = Vec::new();
    let size = BoardEngine::scanout(&mut e, 0, &mut px).unwrap();
    if let Some((w, h)) = size {
        assert_eq!(px.len(), w as usize * h as usize);
    }
    assert!(px.iter().all(|p| p >> 24 == 0), "0x00RRGGBB");
    assert_eq!(BoardEngine::scanout(&mut e, 1, &mut px), Ok(None));
    assert!(px.is_empty());
    assert!(BoardEngine::lend_frame(&mut e, 0).unwrap().is_none());
    e.fail("gone");
    assert_eq!(BoardEngine::scanout(&mut e, 0, &mut px), Err(sgi_engine::Fault("gone".into())));
}

/// A swap latched for the next frame is part of the saved state: a save between the swap and its
/// retrace (the VM stops there and nothing releases it) keeps it, and the loaded engine shows it at
/// its next frame as the saved one would. It was refused before, so savevm and migration failed.
#[test]
fn a_swap_latched_for_the_next_frame_is_saved_and_shown_by_the_loaded_engine() {
    let mut e = engine();
    kernel(&mut e, DISPLAY_MODE_BYTE, &SWAP);
    assert!(e.swap_waiting());
    assert_eq!(e.quiesce(u64::MAX), Ok(()), "nothing is queued behind the swap");
    let mut chunks: Vec<Chunk> = Vec::new();
    e.save(&mut chunks).expect("a save with a swap waiting");

    let mut f = engine();
    f.quiesce(u64::MAX).unwrap();
    f.load(&mut VecSource::new(chunks.clone())).expect("load");
    assert!(f.swap_waiting(), "the swap waits in the loaded engine too");
    let mut again: Vec<Chunk> = Vec::new();
    f.save(&mut again).unwrap();
    assert_eq!(again, chunks, "save, load, save");
    f.resume();
    e.resume();
    f.frame(0);
    e.frame(0);
    assert!(!f.swap_waiting() && !e.swap_waiting());
    assert_eq!(f.compose().2, e.compose().2, "the swap shows the same frame");
}

/// The generation moves when stores are written and again when they run (the picture follows the
/// run, not the write), and holds while the pipe is idle.
#[test]
fn the_frame_generation_moves_with_stores_and_runs_and_holds_when_idle() {
    let mut e = engine();
    let g = BoardEngine::frame_generation(&e, 0);
    assert_ne!(g, 0);
    assert_eq!((BoardEngine::frame_generation(&e, 0), BoardEngine::frame_generation(&e, 1)), (g, 0), "nothing happened; no second head");
    store(&mut e, COMMAND_PAGE, 1, A);
    let written = BoardEngine::frame_generation(&e, 0);
    assert_ne!(written, g, "a store");
    e.run(usize::MAX);
    let ran = BoardEngine::frame_generation(&e, 0);
    assert_ne!(ran, written, "and its run");
    assert_eq!(e.run(usize::MAX), 0);
    assert_eq!(BoardEngine::frame_generation(&e, 0), ran, "an idle pipe holds");
    assert_eq!(BoardEngine::compose_into(&mut e, 0, &0u8, 1280), Ok(None), "no GPU scan-out, nothing to compose into");
}
