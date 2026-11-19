//! The engine's interface from a shell's side, in inline mode. The credit values come from the
//! port of QEMU's rule (a charged store's credit goes to its sender when the pipe has taken its
//! words), not from a pipe.

use std::sync::Arc;

use iris_kona::dev::kona::defs::*;
use iris_kona::dev::kona::engine::{Engine, Options, Out, PipeMemory, Read, Run, Source, Write};
use iris_kona::dev::kona::ident::KonaConfig;
use parking_lot::Mutex;

#[derive(Default)]
struct Ram(Mutex<Vec<u8>>);

impl PipeMemory for Ram {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let m = self.0.lock();
        buf.iter_mut().enumerate().for_each(|(i, b)| *b = m.get(addr as usize + i).copied().unwrap_or(0));
        true
    }

    fn write(&self, addr: u64, data: &[u8]) -> bool {
        let mut m = self.0.lock();
        if m.len() < addr as usize + data.len() {
            m.resize(addr as usize + data.len(), 0);
        }
        m[addr as usize..addr as usize + data.len()].copy_from_slice(data);
        true
    }
}

fn engine() -> Engine {
    Engine::new(KonaConfig::ir2(2).unwrap().with_pool(0, 32), Arc::new(Ram::default()), Options::default()).expect("engine")
}

fn store(e: &mut Engine, off: u32, val: u32, src: Source) {
    assert_eq!(e.write(off, 4, val as u64, src), Write::Done);
}

fn drain(e: &Engine) -> Vec<Out> {
    let mut v = Vec::new();
    e.drain_out(&mut v);
    v
}

const A: Source = Source { hub: Some(0), charged: true };
const B: Source = Source { hub: Some(1), charged: true };

/// The textport packet `1, 9, 0`: a sync word, a selector that takes one word, and that word.
fn textport_packet(e: &mut Engine, senders: [Source; 3]) {
    for (w, s) in [1, 9, 0].into_iter().zip(senders) {
        store(e, COMMAND_PAGE, w, s);
        e.run(usize::MAX);
    }
}

#[test]
fn a_packets_credits_return_with_its_last_word_to_that_words_sender() {
    let mut e = engine();
    textport_packet(&mut e, [A, B, A]);
    // The sync word is taken at once (1 credit to A); the selector opens the packet, which holds
    // its credit; the last word returns its own and the held one (2) to its sender, A.
    assert_eq!(drain(&e), [Out::Credits { hub: Some(0), credits: 1 }, Out::Credits { hub: Some(0), credits: 2 }]);
    textport_packet(&mut e, [B, B, B]);
    assert_eq!(drain(&e), [Out::Credits { hub: Some(1), credits: 1 }, Out::Credits { hub: Some(1), credits: 2 }]);
}

#[test]
fn a_store_nobody_charged_earns_no_credit() {
    let mut e = engine();
    textport_packet(&mut e, [Source::UNCHARGED, A, Source::UNCHARGED]);
    // Only the charged middle word counts; the last word returns it, with no sender to name.
    assert_eq!(drain(&e), [Out::Credits { hub: None, credits: 1 }]);
}

#[test]
fn credits_wait_while_the_driver_has_parked_the_target() {
    let mut e = engine();
    store(&mut e, LOW_WINDOW_WRITE, 1, A);
    textport_packet(&mut e, [A, A, A]);
    assert_eq!(drain(&e), [], "parked credits do not go back");
    // The three held credits and the parking store's own go to the store that clears the bit, and
    // that store's own credit follows.
    store(&mut e, LOW_WINDOW_WRITE, 0x22, B);
    assert_eq!(drain(&e), [Out::Credits { hub: Some(1), credits: 4 }, Out::Credits { hub: Some(1), credits: 1 }]);
}

/// A kernel command: its token by a slot store (the first word), the rest at the port.
fn kernel(e: &mut Engine, token: u32, words: &[u32]) {
    store(e, COMMAND_PAGE + (token << COMMAND_SLOT_SHIFT) + COMMAND_DATA_SLOT, words[0], A);
    for &w in &words[1..] {
        store(e, COMMAND_PAGE, w, A);
    }
    assert_eq!(e.run(usize::MAX), Run::Idle);
}

fn video_interrupts(out: &[Out]) -> usize {
    out.iter().filter(|o| matches!(o, Out::Interrupt { .. })).count()
}

#[test]
fn a_retrace_completes_at_the_frame_the_shell_supplies_and_a_swap_shows_first() {
    let mut e = engine();
    // A destination, the vectors and the video cause enabled, as the driver programs them.
    store(&mut e, INT_DEST_LOWER, 0x1000, A);
    store(&mut e, INT_VECTORS, 0x20000 | 0x11, A);
    store(&mut e, INT_ENABLE, CAUSE_VIDEO, A);

    kernel(&mut e, DISPLAY_MODE_BYTE, &[0x3c0001, 13, 1]);
    assert!(e.status().swap_waiting, "the swap waits for a retrace");
    kernel(&mut e, PUBLISH_RETRACE_SEQUENCE_NUMBER, &[0x1234]);
    assert_eq!(e.read(RETRACE_TAG), Read::Value(0), "the tag changes at the retrace, not at the request");
    assert_eq!(video_interrupts(&drain(&e)), 0);

    e.frame();
    assert!(!e.status().swap_waiting);
    assert_eq!(e.read(RETRACE_TAG), Read::Value(0x1234));
    let out = drain(&e);
    assert_eq!(video_interrupts(&out), 1, "{out:?}");
}

#[test]
fn a_swap_with_latching_off_shows_when_its_command_is_reached() {
    let mut e = Engine::new(KonaConfig::ir2(1).unwrap().with_pool(0, 32), Arc::new(Ram::default()), Options { latch_swaps: false, ..Options::default() }).unwrap();
    kernel(&mut e, DISPLAY_MODE_BYTE, &[0x3c0001, 13, 1]);
    assert!(!e.status().swap_waiting);
}

#[test]
fn two_engines_share_nothing() {
    let (mut a, mut b) = (engine(), engine());
    store(&mut a, INT_VECTORS, 0x5a, A);
    a.run(usize::MAX);
    assert_eq!(b.read(INT_VECTORS), Read::Value(0));
    assert_eq!(a.read(INT_VECTORS), Read::Value(0x5a));
    textport_packet(&mut a, [A, A, A]);
    assert!(drain(&b).is_empty());
}

#[test]
fn a_read_of_a_register_the_stream_changes_waits_for_the_stream() {
    let mut e = engine();
    store(&mut e, COMMAND_PAGE, 1, A);
    assert_eq!(e.read(PORT_ADDR), Read::Wait);
    e.run(usize::MAX);
    assert_eq!(e.read(PORT_ADDR), Read::Value(0));
}

#[test]
fn a_full_stream_answers_busy_and_takes_the_retry() {
    let mut e = Engine::new(KonaConfig::ir2(1).unwrap().with_pool(0, 32), Arc::new(Ram::default()), Options { queue: 4, ..Options::default() }).unwrap();
    for _ in 0..4 {
        assert_eq!(e.write(COMMAND_PAGE, 4, 1, A), Write::Done);
    }
    assert_eq!(e.write(COMMAND_PAGE, 4, 1, A), Write::Busy);
    assert_eq!(e.run(2), Run::More);
    assert_eq!(e.write(COMMAND_PAGE, 4, 1, A), Write::Done);
}

#[test]
fn a_snapshot_taken_after_quiescing_loads_into_a_new_engine() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("kona-engine-snapshot");
    let _ = std::fs::remove_dir_all(&dir);
    let cs = iris_kona::chunk_store::ChunkStore::new(&dir);
    let mut e = engine();
    store(&mut e, INT_VECTORS, 0x77, A);
    e.run(usize::MAX);
    textport_packet(&mut e, [A, A, A]);
    e.quiesce();
    let chunks = e.save_chunks(&cs).expect("a snapshot between commands");
    e.resume();

    let mut f = engine();
    f.load_chunks(&cs, &chunks).expect("load");
    assert_eq!(f.read(INT_VECTORS), Read::Value(0x77));
    assert_eq!(f.compose().2, e.compose().2);

    // Mid-command, a snapshot is refused.
    store(&mut e, COMMAND_PAGE, 1, A);
    store(&mut e, COMMAND_PAGE, 9, A);
    e.run(usize::MAX);
    assert!(e.save_chunks(&cs).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
