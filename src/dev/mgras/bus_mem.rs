//! iris's bus as the command parser's host memory.

use std::sync::Arc;

use super::hostmem::{Fault, HostMem};
use crate::traits::{BusDevice, BUS_OK};

pub struct BusMem(pub Arc<dyn BusDevice>);

impl HostMem for BusMem {
    fn read32(&self, addr: u64) -> Option<u32> {
        let r = self.0.read32(addr);
        r.is_ok().then_some(r.data)
    }

    /// RAM the bus maps directly is read a doubleword at a time; anything
    /// else (and the record/replay wrappers, whose recordings are byte
    /// reads) a byte at a time, 0 where a read fails.
    fn read(&self, addr: u64, out: &mut [u8]) -> Result<(), Fault> {
        let mem = &*self.0;
        let base = addr & !7;
        if mem.mem_ptr(base).is_some() {
            let (mut at, mut head, mut rest) = (base, (addr - base) as usize, &mut *out);
            let (mut words, mut bytes) = ([0u64; 64], [0u8; 512]);
            let mut ok = true;
            while !rest.is_empty() {
                let take = (bytes.len() - head).min(rest.len());
                let n = (head + take).div_ceil(8);
                ok = mem.read_block(at, &mut words[..n]) == BUS_OK;
                if !ok {
                    break;
                }
                for (b, w) in bytes.chunks_exact_mut(8).zip(&words[..n]) {
                    b.copy_from_slice(&w.to_be_bytes());
                }
                let (now, later) = rest.split_at_mut(take);
                now.copy_from_slice(&bytes[head..head + take]);
                (rest, head, at) = (later, 0, at + bytes.len() as u64);
            }
            if ok {
                return Ok(());
            }
        }
        let mut ok = true;
        for (i, b) in out.iter_mut().enumerate() {
            let r = mem.read8(addr + i as u64);
            ok &= r.is_ok();
            *b = if r.is_ok() { r.data } else { 0 };
        }
        if ok { Ok(()) } else { Err(Fault) }
    }

    fn write(&self, addr: u64, data: &[u8]) -> Result<(), Fault> {
        let mut ok = true;
        for (i, b) in data.iter().enumerate() {
            ok &= self.0.write8(addr + i as u64, *b) == BUS_OK;
        }
        if ok { Ok(()) } else { Err(Fault) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{BusRead8, BUS_OK};

    /// RAM the bus maps directly (`mem_ptr`), big-endian: byte `a` at `a`.
    struct Ram(Vec<u8>);

    impl BusDevice for Ram {
        fn read8(&self, addr: u64) -> BusRead8 {
            BusRead8::ok(self.0[addr as usize])
        }
        fn mem_ptr(&self, addr: u64) -> Option<*const u64> {
            Some(self.0[addr as usize..].as_ptr() as *const u64)
        }
        fn read_block(&self, addr: u64, buf: &mut [u64]) -> u32 {
            for (i, w) in buf.iter_mut().enumerate() {
                let at = addr as usize + 8 * i;
                *w = u64::from_be_bytes(self.0[at..at + 8].try_into().unwrap());
            }
            BUS_OK
        }
    }

    /// A read from directly mapped RAM gives the bytes a byte read would,
    /// for every alignment and length within a page.
    #[test]
    fn read_matches_byte_reads() {
        let ram = Arc::new(Ram((0..0x2000u32).map(|i| (i * 7 + (i >> 8)) as u8).collect()));
        let mem = BusMem(ram.clone());
        for pa in [0x1000u64, 0x1001, 0x1003, 0x1007, 0x1008, 0x10F5] {
            for len in [0usize, 1, 5, 7, 8, 9, 16, 23, 64, 0x100, 0x1FF, 0x200, 0x207, 0xF00] {
                let mut fast = vec![0u8; len];
                mem.read(pa, &mut fast).unwrap();
                let slow: Vec<u8> = (0..len as u64).map(|i| ram.read8(pa + i).data).collect();
                assert_eq!(fast, slow, "pa {pa:#x} len {len}");
            }
        }
    }
}
