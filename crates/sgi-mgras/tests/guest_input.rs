//! Inputs a guest or a shell can send that once panicked or never came back: each found by the
//! random-input test (fuzz.rs) and kept as a case of its own.

use sgi_engine::{BoardEngine, HostMem, MemFault, Swap};
use sgi_mgras::dev::mgras::engine::{Board, Config, MgrasEngine, Mode};
use sgi_mgras::dev::mgras::hq3::host;

struct Noise(u64);

impl HostMem for Noise {
    fn ram_read(&self, _: u64, out: &mut [u8], _: Swap) -> Result<(), MemFault> {
        out.fill(self.0 as u8);
        Ok(())
    }

    fn ram_write(&self, _: u64, _: &[u8], _: Swap) -> Result<(), MemFault> {
        Ok(())
    }
}

/// `push_words` names its port by index (0 the user's, 1 the privileged one) or by the window
/// offset `write` takes; a port that is neither is no FIFO and takes nothing. The offset's low
/// byte used to index the parser's two ports, which panicked for a shell that passed 0x70080.
#[test]
fn push_words_takes_a_port_by_index_or_offset_and_refuses_any_other() {
    let mut e = MgrasEngine::new(Config::new(Board::High, Mode::Inline), Noise(1), None, None).unwrap();
    for port in [0, 1, host::CFIFO, host::CFIFO_GL, host::CFIFO_PRIVILEGED, 0x70084, 0x50084] {
        assert_eq!(e.push_words(port, &[0, 0]), 2, "port {port:#x}");
    }
    for port in [2, 0x80, 0x7_0000, 0x7_0100, u32::MAX] {
        assert_eq!(e.push_words(port, &[0, 0]), 0, "port {port:#x}");
    }
}
