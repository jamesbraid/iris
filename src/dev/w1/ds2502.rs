//! DS2501 (512 bits, two 32-byte pages) and DS2502 (1 Kbit, four pages)
//! add-only memories: Read Memory (0xf0), Read Data/Generate CRC (0xc3)
//! and Read Status (0xaa). Writes are refused.

use super::Part;

pub struct Ds2502 {
    _todo: (),
}

impl Ds2502 {
    /// A DS2502 (128 bytes) with family code `family`, the 48-bit
    /// `serial`, and `image` at the start of its memory (the rest erased,
    /// 0xff).
    pub fn new(family: u8, serial: u64, image: &[u8]) -> Ds2502 {
        let _ = (family, serial, image);
        todo!()
    }

    /// A DS2501 (64 bytes), otherwise as `new`.
    pub fn ds2501(family: u8, serial: u64, image: &[u8]) -> Ds2502 {
        let _ = (family, serial, image);
        todo!()
    }
}

impl Part for Ds2502 {
    fn rom(&self) -> [u8; 8] {
        todo!()
    }

    fn reply(&self, cmd: u8, addr: u16) -> Option<Vec<u8>> {
        let _ = (cmd, addr);
        todo!()
    }
}
