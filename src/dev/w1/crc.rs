//! The Dallas/Maxim CRCs the parts frame their streams with.

/// CRC-8 (x^8 + x^5 + x^4 + 1, reflected), as in a ROM's last byte. A
/// ROM's CRC over all eight bytes is zero.
pub fn crc8(data: &[u8]) -> u8 {
    let _ = data;
    todo!()
}

/// CRC-16 (x^16 + x^15 + x^2 + 1, reflected) from `seed`, as DS2505-class
/// parts and SGI's board records compute it. Parts send it inverted.
pub fn crc16(seed: u16, data: &[u8]) -> u16 {
    let _ = (seed, data);
    todo!()
}
