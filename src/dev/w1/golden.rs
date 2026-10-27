//! Reply streams equal to what the earlier per-master 1-wire code produced.
//! The fixtures were generated once by running that code (the bus and parts
//! the IOC3 module carried) over the same parts and driver, and pasted here
//! as lengths, FNV-1a digests and head bytes.
//!
//! Three things changed on purpose, and the fixtures account for them: the
//! CRC in front of (or seeded by) a start address above the top covers the
//! address as masked, so those cases were generated at the masked address;
//! a DS2501 now answers the DS2502's 128-byte address space (the part's
//! 64 programmable bytes, then erased), so its fixtures stop where the old
//! 64-byte stream ended; and a DS2502 Read Status start above 7 is no longer
//! masked, so it has no fixture (see `tests.rs`).

use super::*;

trait Wire {
    fn reset(&mut self) -> bool;
    fn slot(&mut self, bit: bool) -> bool;
}

fn wr(w: &mut impl Wire, b: u8) {
    for i in 0..8 {
        w.slot(b >> i & 1 != 0);
    }
}

fn rd(w: &mut impl Wire) -> u8 {
    (0..8).fold(0, |a, i| a | (w.slot(true) as u8) << i)
}

/// Skip ROM, a memory command with its address, then `n` bytes read.
fn stream(w: &mut impl Wire, cmd: u8, ta: u16, n: usize) -> Vec<u8> {
    assert!(w.reset());
    wr(w, 0xcc);
    wr(w, cmd);
    wr(w, ta as u8);
    wr(w, (ta >> 8) as u8);
    (0..n).map(|_| rd(w)).collect()
}

fn fnv(d: &[u8]) -> u64 {
    d.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// A Search ROM walk: the level of each (bit, complement) pair and the
/// master's choice, as 0/1 characters; then a Read Memory at 0 from the
/// part it leaves selected, four bytes. `policy` 0/1 takes that branch at a
/// discrepancy; 2+k follows part k's ROM.
fn search_trace(w: &mut impl Wire, policy: usize, roms: &[[u8; 8]]) -> String {
    assert!(w.reset());
    wr(w, 0xf0);
    let mut s = String::new();
    for i in 0..64 {
        let b = w.slot(true);
        let nb = w.slot(true);
        let choice = match (b, nb) {
            (false, false) => match policy {
                0 => false,
                1 => true,
                k => roms[k - 2][i / 8] >> (i % 8) & 1 != 0,
            },
            (true, true) => panic!("nobody answered bit {i}"),
            (b, _) => b,
        };
        w.slot(choice);
        s.push(if b { '1' } else { '0' });
        s.push(if nb { '1' } else { '0' });
        s.push(if choice { '1' } else { '0' });
    }
    wr(w, 0xf0);
    wr(w, 0);
    wr(w, 0);
    let got: Vec<u8> = (0..4).map(|_| rd(w)).collect();
    format!("{s}|{}", hex(&got))
}

/// Read ROM, Skip ROM Read Memory, and a Match ROM with one bit wrong, all
/// on the same multi-part wire.
fn broadcast_trace(w: &mut impl Wire, roms: &[[u8; 8]]) -> String {
    assert!(w.reset());
    wr(w, 0x33);
    let rom: Vec<u8> = (0..8).map(|_| rd(w)).collect();
    let skip = stream(w, 0xf0, 0, 16);
    assert!(w.reset());
    wr(w, 0x55);
    let mut bad = roms[0];
    bad[3] ^= 1;
    for b in bad {
        wr(w, b);
    }
    wr(w, 0xf0);
    wr(w, 0);
    wr(w, 0);
    let nobody: Vec<u8> = (0..2).map(|_| rd(w)).collect();
    format!("{}|{}|{}", hex(&rom), hex(&skip), hex(&nobody))
}

const N: usize = 2420;

/// FNV-1a of the first `n` bytes of the stream, and its first 24 bytes.
fn digest(w: &mut impl Wire, cmd: u8, ta: u16, n: usize) -> (u64, String) {
    let s = stream(w, cmd, ta, n);
    (fnv(&s), hex(&s[..24.min(n)]))
}

// name, cmd, start address, bytes read, fnv64 of them, first 24 bytes
const STREAMS: &[(&str, u8, u16, usize, u64, &str)] = &[
    ("ds2502", 0xf0, 0x0000, 2420, 0x759379ac36b89e69, "8d0104070a0d101316191c1f2225282b2e3134373a3d4043"),
    ("ds2502", 0xf0, 0x0040, 2420, 0xcc58573e64e58d1d, "16c1c4c7cacdd0d3d6d9dcdfe2e5e8ebeef1f4f7fafd0003"),
    ("ds2502", 0xf0, 0x007f, 2420, 0x31c1b2888c00e122, "237ee7ffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0xf0, 0x0080, 2420, 0x759379ac36b89e69, "8d0104070a0d101316191c1f2225282b2e3134373a3d4043"),
    ("ds2502", 0xf0, 0x01c5, 2420, 0xece0db9e4b82a4ff, "e9d0d3d6d9dcdfe2e5e8ebeef1f4f7fafd000306090c0f12"),
    ("ds2502", 0xf0, 0x0003, 2420, 0xf59d04b26a0cdd26, "d80a0d101316191c1f2225282b2e3134373a3d404346494c"),
    ("ds2502", 0xf0, 0x0007, 2420, 0x983af73cc07d4f0d, "e316191c1f2225282b2e3134373a3d404346494c4f525558"),
    ("ds2502", 0xf0, 0x0008, 2420, 0x8f0a2f28f16eb9b7, "fb191c1f2225282b2e3134373a3d404346494c4f5255585b"),
    ("ds2502", 0xc3, 0x0000, 2420, 0x9b0a60c9ec0ac10b, "b70104070a0d101316191c1f2225282b2e3134373a3d4043"),
    ("ds2502", 0xc3, 0x0040, 2420, 0xe41dddd6ef067e42, "2cc1c4c7cacdd0d3d6d9dcdfe2e5e8ebeef1f4f7fafd0003"),
    ("ds2502", 0xc3, 0x007f, 2420, 0x6e827f68f554275c, "197ee7ffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0xc3, 0x0080, 2420, 0x9b0a60c9ec0ac10b, "b70104070a0d101316191c1f2225282b2e3134373a3d4043"),
    ("ds2502", 0xc3, 0x01c5, 2420, 0x54b2d0923a32bccf, "d3d0d3d6d9dcdfe2e5e8ebeef1f4f7fafd000306090c0f12"),
    ("ds2502", 0xc3, 0x0003, 2420, 0xae37348d2c76b278, "e20a0d101316191c1f2225282b2e3134373a3d404346494c"),
    ("ds2502", 0xc3, 0x0007, 2420, 0x99b55abcef47c9de, "d916191c1f2225282b2e3134373a3d404346494c4f525558"),
    ("ds2502", 0xc3, 0x0008, 2420, 0xd472bed2057e4c6d, "c1191c1f2225282b2e3134373a3d404346494c4f5255585b"),
    ("ds2502", 0xaa, 0x0000, 2420, 0xeec80c7b4da3d930, "9cffffffffffffff00fcffffffffffffffffffffffffffff"),
    ("ds2502", 0xaa, 0x0003, 2420, 0x874dfe58929d1ac8, "c9ffffffff0071ffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0xaa, 0x0007, 2420, 0x1657f0ad38674196, "f20000ffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0x0f, 0x0000, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0x0f, 0x0040, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0x0f, 0x007f, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0x0f, 0x0080, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0x0f, 0x01c5, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0x0f, 0x0003, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0x0f, 0x0007, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0x0f, 0x0008, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0x55, 0x0000, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0x55, 0x0040, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0x55, 0x007f, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0x55, 0x0080, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0x55, 0x01c5, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0x55, 0x0003, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0x55, 0x0007, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0x55, 0x0008, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0xa5, 0x0000, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0xa5, 0x0040, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0xa5, 0x007f, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0xa5, 0x0080, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0xa5, 0x01c5, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0xa5, 0x0003, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0xa5, 0x0007, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2502", 0xa5, 0x0008, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2501", 0xf0, 0x0000, 65, 0x85a328b18cf42978, "8d02070c11161b20252a2f34393e43484d52575c61666b70"),
    ("ds2501", 0xf0, 0x0020, 33, 0x76388bcbea60f7eb, "4ca2a7acb1b6bbc0c5cacfd4d9dee3e8edf2f7fc01060b10"),
    ("ds2501", 0xf0, 0x003f, 2, 0x09ca8b07b64641ce, "b83d"),
    ("ds2501", 0xf0, 0x0003, 62, 0xec746aedbc1aec46, "d811161b20252a2f34393e43484d52575c61666b70757a7f"),
    ("ds2501", 0xc3, 0x0000, 67, 0x40b81739158a51bf, "b702070c11161b20252a2f34393e43484d52575c61666b70"),
    ("ds2501", 0xc3, 0x0020, 34, 0xcd89fc0b1d6b1256, "76a2a7acb1b6bbc0c5cacfd4d9dee3e8edf2f7fc01060b10"),
    ("ds2501", 0xc3, 0x003f, 3, 0x400a9d1aea715c11, "823d43"),
    ("ds2501", 0xc3, 0x0003, 64, 0x77edb83fbe59e380, "e211161b20252a2f34393e43484d52575c61666b70757a7f"),
    ("ds2501", 0xaa, 0x0000, 2420, 0xeec80c7b4da3d930, "9cffffffffffffff00fcffffffffffffffffffffffffffff"),
    ("ds2501", 0xaa, 0x0003, 2420, 0x874dfe58929d1ac8, "c9ffffffff0071ffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf0, 0x0000, 2420, 0x65469d858abb0def, "0910171e252c333a41484f565d646b727980878e959ca3aa"),
    ("ds2505", 0xf0, 0x0045, 2420, 0x885beba33ef1dcf2, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf0, 0x0100, 2420, 0x72c898dc37f1ac2f, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf0, 0x013f, 2420, 0xcd224fc8781e954c, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf0, 0x07e0, 2420, 0x4321d37b35a99e54, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf0, 0x07f9, 2420, 0x4f4fa7ad21d50514, "ffffffffffffffdb0effffffffffffffffffffffffffffff"),
    ("ds2505", 0xf0, 0x07ff, 2420, 0x3886bdfe47c25e36, "ffbebfffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf0, 0x0800, 2420, 0x65469d858abb0def, "0910171e252c333a41484f565d646b727980878e959ca3aa"),
    ("ds2505", 0xf0, 0x0a45, 2420, 0x916bb21bfb4554c9, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xaa, 0x0000, 2420, 0xd1ef3532e9d0c33c, "ffffffffffffffff9da1ffffffffffffffffbe7bffffffff"),
    ("ds2505", 0xaa, 0x0045, 2420, 0xc106fdfd7abb8d87, "ffffff1bbaffffffffffffffffbe7bffffffffffffffffbe"),
    ("ds2505", 0xaa, 0x0100, 2420, 0x0d9ced4413785e95, "ffffffffffffffff9031ffffffffffffffffbe7bffffffff"),
    ("ds2505", 0xaa, 0x013f, 2420, 0x90e38dee0bbadeb5, "ffaffbffffffffffffffffbe7bffffffffffffffffbe7bff"),
    ("ds2505", 0xaa, 0x07e0, 2420, 0xe4ea65fe804d2f04, "ffffffffffffffffbfc7ffffffffffffffffbe7bffffffff"),
    ("ds2505", 0xaa, 0x07f9, 2420, 0x7ab2ecc11e510a3c, "fffffffffffffff86fffffffffffffffffffffffffffffff"),
    ("ds2505", 0xaa, 0x07ff, 2420, 0x851235987bb70ef4, "ffac67ffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xaa, 0x0800, 2420, 0xd1ef3532e9d0c33c, "ffffffffffffffff9da1ffffffffffffffffbe7bffffffff"),
    ("ds2505", 0xaa, 0x0a45, 2420, 0x77d0e16b3f038470, "ffffff1a02ffffffffffffffffbe7bffffffffffffffffbe"),
    ("ds2505", 0xa5, 0x0000, 2420, 0xeb0fdfa03db9af37, "ff9d730910171e252c333a41484f565d646b727980878e95"),
    ("ds2505", 0xa5, 0x0045, 2420, 0x3a6c2d08e317ee65, "ff8ca6ffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xa5, 0x0100, 2420, 0xb4ea00f63777fe98, "ff9ce3ffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xa5, 0x013f, 2420, 0x23dc601f0a81e6f2, "ffacefffbfbfffbfbfffffffffffffffffffffffffffffff"),
    ("ds2505", 0xa5, 0x07e0, 2420, 0xb22c762a76215087, "ff9eb5ffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xa5, 0x07f9, 2420, 0xa6e25b494ec35966, "ff4f72fffffffffffffff17fffffffffffffffffffffffff"),
    ("ds2505", 0xa5, 0x07ff, 2420, 0xdcecb0a627b69f1d, "ffaf73ffbfbfffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xa5, 0x0800, 2420, 0xeb0fdfa03db9af37, "ff9d730910171e252c333a41484f565d646b727980878e95"),
    ("ds2505", 0xa5, 0x0a45, 2420, 0x1f52d1c1ea5fb4b2, "ff8dc6ffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xc3, 0x0000, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xc3, 0x0045, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xc3, 0x0100, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xc3, 0x013f, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xc3, 0x07e0, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xc3, 0x07f9, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xc3, 0x07ff, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xc3, 0x0800, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xc3, 0x0a45, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0x0f, 0x0000, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0x0f, 0x0045, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0x0f, 0x0100, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0x0f, 0x013f, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0x0f, 0x07e0, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0x0f, 0x07f9, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0x0f, 0x07ff, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0x0f, 0x0800, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0x0f, 0x0a45, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf3, 0x0000, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf3, 0x0045, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf3, 0x0100, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf3, 0x013f, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf3, 0x07e0, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf3, 0x07f9, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf3, 0x07ff, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf3, 0x0800, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf3, 0x0a45, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0x55, 0x0000, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0x55, 0x0045, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0x55, 0x0100, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0x55, 0x013f, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0x55, 0x07e0, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0x55, 0x07f9, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0x55, 0x07ff, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0x55, 0x0800, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0x55, 0x0a45, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf5, 0x0000, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf5, 0x0045, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf5, 0x0100, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf5, 0x013f, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf5, 0x07e0, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf5, 0x07f9, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf5, 0x07ff, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf5, 0x0800, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
    ("ds2505", 0xf5, 0x0a45, 2420, 0xee65e5922f97cfa1, "ffffffffffffffffffffffffffffffffffffffffffffffff"),
];
const SEARCHES: &[&str] = &[
    "101000010000101010010101010010010010010010010010010010010010101010101010101010101010101010101101010010101010101010101010010010101010101101101010010101101101101010101010010010010101010101101101|8d02070c",
    "101001010101010010010010010010010010010010010010010101101010010010101010101010101010101010101010101010101101010010101010101010101010010010101010101010010010101010101010010101010101101010101010|0910171e",
    "101000010001010010010010101101010101010101010101101010010101010010010101101101101010010101101010101010101010010010101010101101010010010101010010101010010010010010010010010010010101010010010101|8d010407",
    "101000010000101010010101010010010010010010010010010010010010101010101010101010101010101010101101010010101010101010101010010010101010101101101010010101101101101010101010010010010101010101101101|8d02070c",
    "101001010101010010010010010010010010010010010010010101101010010010101010101010101010101010101010101010101101010010101010101010101010010010101010101010010010101010101010010101010101101010101010|0910171e",
];
const BROADCAST: &str = "0100004544000008|090004040004100000080c1400240000|ffff";

impl Wire for Bus {
    fn reset(&mut self) -> bool {
        Bus::reset(self)
    }
    fn slot(&mut self, bit: bool) -> bool {
        Bus::slot(self, bit)
    }
}

fn pat(n: usize, m: usize, a: usize) -> Vec<u8> {
    (0..n).map(|i| (i * m + a) as u8).collect()
}

/// The three parts the fixtures were generated with.
fn parts() -> [(&'static str, Box<dyn Part>); 3] {
    [
        ("ds2502", Box::new(Ds2502::new(0x09, 0x0123_4567_89ab, &pat(128, 3, 1)))),
        ("ds2501", Box::new(Ds2502::ds2501(0x91, 0x5e74_54d5_5000, &pat(64, 5, 2)))),
        ("ds2505", Box::new(Ds2505::new(0x0b, 0x5145_4d55_4600, &pat(64, 7, 9)))),
    ]
}

fn part(name: &str) -> Box<dyn Part> {
    parts().into_iter().find(|(n, _)| *n == name).unwrap().1
}

fn wire() -> Bus {
    let mut b = Bus::new();
    for (_, p) in parts() {
        b.add(p);
    }
    b
}

#[test]
fn reply_streams_match_the_earlier_code_byte_for_byte() {
    for &(name, cmd, ta, n, want, head) in STREAMS {
        let mut b = Bus::new();
        b.add(part(name));
        let (got, got_head) = digest(&mut b, cmd, ta, n);
        assert_eq!((got_head.as_str(), got), (head, want), "{name} {cmd:#04x} at {ta:#06x}");
    }
}

#[test]
fn search_rom_on_three_parts_matches_the_earlier_code() {
    let roms = wire().roms();
    for (policy, want) in SEARCHES.iter().enumerate() {
        assert_eq!(search_trace(&mut wire(), policy, &roms), *want, "policy {policy}");
    }
}

#[test]
fn read_rom_skip_rom_and_a_bad_match_on_three_parts_match_the_earlier_code() {
    let roms = wire().roms();
    assert_eq!(broadcast_trace(&mut wire(), &roms), BROADCAST);
}
