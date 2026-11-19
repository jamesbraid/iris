//! The back end under a front end with no thread and no machine: what a drawing test drives with
//! `Job`s, over a RAM it can look at, without iris's board or its test harness.
//!
//! The front end is the pipe's own state (`Core`) with the return page and the reason page
//! registered and context 0 current, as the board's tests set it up.

use std::sync::Arc;

use parking_lot::Mutex;

use super::back::KonaBackend;
use super::flash::Flash;
use super::front::{Front, Host};
use super::ident::KonaConfig;
use super::job::{BackPort, Grammar, Job, Reply};
use super::pipe::Core;
use crate::irq::InterruptController;
use crate::traits::{BusDevice, BUS_ERR, BUS_OK};

/// The page the pipe answers queries and reads on, and the page it posts reasons on.
pub const RETURN_PAGE: u64 = 0x30_0000;
pub const REASON_PAGE: u64 = 0x31_0000;
const RAM: usize = 16 << 20;

/// Guest memory, big-endian words, and an interrupt controller that takes the packets.
struct Ram(Mutex<Vec<u8>>);

impl InterruptController for Ram {
    fn set_line(&self, _line: u32, _level: bool) {}
}

impl BusDevice for Ram {
    fn read_bytes(&self, addr: u64, buf: &mut [u8]) -> u32 {
        let ram = self.0.lock();
        match ram.get(addr as usize..addr as usize + buf.len()) {
            Some(s) => {
                buf.copy_from_slice(s);
                BUS_OK
            }
            None => BUS_ERR,
        }
    }

    fn write_bytes(&self, addr: u64, data: &[u8]) -> u32 {
        let mut ram = self.0.lock();
        match ram.get_mut(addr as usize..addr as usize + data.len()) {
            Some(s) => {
                s.copy_from_slice(data);
                BUS_OK
            }
            None => BUS_ERR,
        }
    }
}

/// A back end and the front end behind it.
pub struct Rig {
    pub back: KonaBackend,
    host: Host,
    core: Core,
    ram: Arc<Ram>,
}

impl Rig {
    /// A pipe of configuration `cfg` on the CPU.
    pub fn new(cfg: KonaConfig) -> Rig {
        let ram = Arc::new(Ram(Mutex::new(vec![0; RAM])));
        let host = Host::new(Front::new(&cfg), ram.clone(), ram.clone());
        let mut core = Core::new(cfg, Flash::fresh());
        core.current = Some(0);
        core.return_page = Some(RETURN_PAGE);
        core.reason_page = Some(REASON_PAGE);
        Rig { back: KonaBackend::with_gpu(cfg, None).expect("a CPU back end"), host, core, ram }
    }

    /// One job through the back end.
    pub fn send(&mut self, job: Job<'_>) -> Reply {
        self.back.job(&mut BackPort { core: &mut self.core, host: &self.host }, job)
    }

    /// A command the front end framed, in `grammar`.
    pub fn command(&mut self, grammar: Grammar, selector: u32, args: &[u32]) -> Reply {
        self.send(Job::Command { grammar, selector, args, packed: 0, handled: false })
    }

    /// An X server command (the kernel's grammar).
    pub fn x(&mut self, selector: u32, args: &[u32]) -> Reply {
        self.command(Grammar::Kernel, selector, args)
    }

    /// Word `w` of the pixel at (x, y), after the drawing behind it has run.
    pub fn read(&mut self, x: u32, y: u32, w: usize) -> u32 {
        self.back.drain();
        self.back.pixels().word(x, y, w)
    }

    pub fn put(&self, addr: u64, bytes: &[u8]) {
        self.ram.0.lock()[addr as usize..addr as usize + bytes.len()].copy_from_slice(bytes);
    }

    pub fn put_words(&self, addr: u64, words: &[u32]) {
        for (i, w) in words.iter().enumerate() {
            self.put(addr + 4 * i as u64, &w.to_be_bytes());
        }
    }

    pub fn bytes(&self, addr: u64, len: usize) -> Vec<u8> {
        self.ram.0.lock()[addr as usize..addr as usize + len].to_vec()
    }

    pub fn word(&self, addr: u64) -> u32 {
        u32::from_be_bytes(self.bytes(addr, 4).try_into().unwrap())
    }

    /// The context a job runs under: None is no context current, which answers nothing.
    pub fn set_current(&mut self, slot: Option<usize>) {
        self.core.current = slot;
    }

    /// Whether the last DMA read from host memory failed.
    pub fn dma_fault(&self) -> bool {
        self.core.dma_fault
    }

    /// The pipe's map RAM entry `index`, which the host buffers a DMA names are mapped through.
    pub fn map_ram(&mut self) -> &mut [u32] {
        &mut self.core.map_ram
    }
}
