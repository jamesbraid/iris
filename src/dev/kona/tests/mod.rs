//! Register-contract tests of the pipe's front end.
//!
//! The cases are ported from the QEMU model's qtests (bring-up, config,
//! EEPROM and DMA); a doc comment names the case a test comes from. They
//! reach the board the way a machine does, through `bus_read` and
//! `bus_write`, with the three attach handles played by `TestHost`. With no
//! thread running a stream store runs in the store, which is how a test sees
//! its effect at once; the threaded cases use `TestBoard` and retry on busy.

use super::*;
use crate::test_board::TestBoard;
use std::sync::atomic::Ordering;

/// A back end that records every job and answers as the test says.
#[derive(Default)]
pub(super) struct Recorder {
    pub jobs: Arc<Mutex<Vec<String>>>,
    /// Data words a command is to be followed by, by selector.
    pub data: Vec<(u32, u32)>,
    pub unclaimed: bool,
    pub resets: u32,
}

impl Backend for Recorder {
    fn job(&mut self, port: &mut BackPort<'_>, job: Job<'_>) -> Reply {
        let line = match &job {
            Job::Command { grammar, selector, args, packed, handled } => {
                format!("{grammar:?} {selector:#x} {args:x?} packed={packed:#x} handled={handled}")
            }
            Job::Data { grammar, selector, args, words } => format!("data {grammar:?} {selector:#x} {args:x?} {words:x?}"),
            Job::Bitmap { header, payload } => format!("bitmap {header:x?} {payload:x?}"),
            Job::Video { command, data } => format!("video {command:#x} {data:x?}"),
            Job::Retrace { tag } => format!("retrace {tag:#x}"),
            Job::Context(e) => format!("context {e:?}"),
            Job::Indirect { selector, value } => format!("indirect {selector:#x} {value:#x}"),
            Job::DmaIn { io, physical, lines, per_line, pitch } => {
                format!("dma-in {io:#x} {lines} {per_line:#x} {pitch:#x}{}", if *physical { " physical" } else { "" })
            }
            Job::FirmwareStart { combination } => format!("firmware {combination:x?}"),
        };
        self.jobs.lock().push(line);
        match job {
            Job::Retrace { tag } => port.retrace_done(tag),
            Job::Command { selector, handled: false, .. } => {
                if let Some(&(_, n)) = self.data.iter().find(|(s, _)| *s == selector) {
                    return Reply::Data(n);
                }
            }
            Job::DmaIn { .. } if self.unclaimed => return Reply::Unclaimed,
            _ => {}
        }
        Reply::Done
    }

    fn reset(&mut self) {
        self.resets += 1;
    }
}

pub(super) struct Fixture {
    pub host: Arc<TestHost>,
    pub kona: Arc<Kona>,
    pub jobs: Arc<Mutex<Vec<String>>>,
}

impl Fixture {
    /// An IR2 with a recording back end and no thread.
    pub fn new() -> Fixture {
        Fixture::with(KonaConfig::default(), Recorder::default())
    }

    pub fn with(cfg: KonaConfig, rec: Recorder) -> Fixture {
        let host = TestHost::new();
        let jobs = rec.jobs.clone();
        let kona = KonaBuilder::new(cfg, attach(&host)).backend(Box::new(rec)).build().unwrap();
        Fixture { host, kona, jobs }
    }

    /// A store of one word, retried while the bus answers busy: the
    /// processor retries a refused store, so a test does.
    pub fn w(&self, off: u64, val: u32) {
        retry(|| self.kona.bus_write(off, 4, val as u64) == BUS_OK);
    }

    pub fn w64(&self, off: u64, val: u64) {
        retry(|| self.kona.bus_write(off, 8, val) == BUS_OK);
    }

    /// A load of one word, retried while busy.
    pub fn r(&self, off: u64) -> u32 {
        let mut v = 0;
        retry(|| {
            let (s, d) = self.kona.bus_read(off, 4);
            v = d as u32;
            s == BUS_OK
        });
        v
    }

    /// Words to the command port.
    pub fn cmd(&self, words: &[u32]) {
        for &w in words {
            self.w(COMMAND_PORT, w);
        }
    }

    pub fn jobs(&self) -> Vec<String> {
        self.jobs.lock().clone()
    }

    pub fn take_jobs(&self) -> Vec<String> {
        std::mem::take(&mut *self.jobs.lock())
    }

    /// Map RAM entry `i` to the 16 KiB host frame at `host_addr`.
    pub fn map(&self, i: u64, host_addr: u64) {
        self.w(0x100000 + 4 * i, 0x2000_0000 | (host_addr >> 12) as u32);
    }

    /// The kernel's reason-page registration with a physical page.
    pub fn reason_page(&self, addr: u64) {
        self.cmd(&[0x1f1, 0x1000_0000 | (addr >> 12) as u32, (addr & 0xfff) as u32, 0x4000]);
    }

    pub fn return_page(&self, addr: u64) {
        self.cmd(&[0x1f2, 0x1000_0000 | (addr >> 12) as u32, (addr & 0xfff) as u32, 0x4000]);
    }
}

pub(super) const COMMAND_PORT: u64 = 0xc10000;
pub(super) const KICK: u64 = 0xc00108;

/// Retry `f` until it succeeds. A bounded loop, not a wall-clock wait: a
/// store is refused only while the thread works its way through the ring.
pub(super) fn retry(mut f: impl FnMut() -> bool) {
    for _ in 0..50_000_000u32 {
        if f() {
            return;
        }
        std::thread::yield_now();
    }
    panic!("the bus kept answering busy");
}

/// A fixture with the thread running, freed with the test.
pub(super) fn threaded() -> (Fixture, TestBoard<Kona>) {
    let f = Fixture::new();
    let board = TestBoard::new(f.kona.clone());
    board.start();
    (f, board)
}

mod regs;
mod host;
pub(super) use host::{attach, TestHost};

mod bringup;
mod config;
mod eeprom;
mod dma;
mod fifo;
