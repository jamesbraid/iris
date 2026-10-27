//! The bus: which parts are selected, the ROM command phases (Read, Match,
//! Search and Skip ROM), then a memory function command, its two address
//! bytes and the selected parts' reply streams, wired-AND.

/// A part on the bus.
pub trait Part: Send {
    /// The 64-bit registration number: family code, 48-bit serial (least
    /// significant byte first), CRC-8 of the first seven bytes.
    fn rom(&self) -> [u8; 8];

    /// What the part sends after memory function command `cmd` and target
    /// address `addr`, every byte to the end of the stream, CRCs included.
    /// `None` for a command it refuses (writes, anything unknown): the bus
    /// then goes quiet until the next reset.
    fn reply(&self, cmd: u8, addr: u16) -> Option<Vec<u8>>;

    /// Bit `i` (0..64) of the ROM, least significant bit of byte 0 first.
    fn rom_bit(&self, i: usize) -> bool {
        self.rom()[i / 8] >> (i % 8) & 1 != 0
    }
}

/// One wire and the parts on it.
#[derive(Default)]
pub struct Bus {
    _todo: (),
}

impl Bus {
    /// A bus with nothing on it: a reset gets no presence.
    pub fn new() -> Bus {
        todo!()
    }

    /// Put a part on the wire.
    pub fn add(&mut self, part: Box<dyn Part>) {
        let _ = part;
        todo!()
    }

    /// The parts' ROMs, in the order they were added.
    pub fn roms(&self) -> Vec<[u8; 8]> {
        todo!()
    }

    /// A reset pulse: every part selected, the ROM command phase begins.
    /// True if any part answered with presence.
    pub fn reset(&mut self) -> bool {
        todo!()
    }

    /// One bit slot: the master writes `bit` (a one is also a read slot).
    /// Returns the level on the wire, the AND of the master's bit and
    /// every selected part's.
    pub fn slot(&mut self, bit: bool) -> bool {
        let _ = bit;
        todo!()
    }
}
