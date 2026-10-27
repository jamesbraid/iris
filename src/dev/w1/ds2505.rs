//! DS2505 (16 Kbit, 64 pages of 32 bytes) add-only memory: Read Memory
//! (0xf0), Read Status (0xaa) and Extended Read Memory (0xa5). Writes are
//! refused.

use super::Part;

pub struct Ds2505 {
    _todo: (),
}

impl Ds2505 {
    /// A DS2505 with family code `family`, the 48-bit `serial`, and `image`
    /// at the start of its memory (the rest erased, 0xff).
    pub fn new(family: u8, serial: u64, image: &[u8]) -> Ds2505 {
        let _ = (family, serial, image);
        todo!()
    }
}

impl Part for Ds2505 {
    fn rom(&self) -> [u8; 8] {
        todo!()
    }

    fn reply(&self, cmd: u8, addr: u16) -> Option<Vec<u8>> {
        let _ = (cmd, addr);
        todo!()
    }
}
