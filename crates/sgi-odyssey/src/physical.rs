//! `read_bytes` and `write_bytes` of iris's `physical`: guest memory as a
//! DMA master sees it, whole qwords a page at a time and single bytes at
//! the ragged ends. Returns `BUS_OK` or the first other status.

use crate::traits::{BusDevice, BUS_OK};

pub fn read_bytes(bus: &dyn BusDevice, addr: u64, buf: &mut [u8]) -> u32 {
    const PAGE: u64 = 4096;
    let mut done = 0usize;
    let mut words = [0u64; (PAGE / 8) as usize];
    while done < buf.len() {
        let a = addr.wrapping_add(done as u64);
        let left = buf.len() - done;
        if a & 7 != 0 || left < 8 {
            let r = bus.read8(a);
            if !r.is_ok() {
                return r.status;
            }
            buf[done] = r.data;
            done += 1;
            continue;
        }
        let n = (((PAGE - (a & (PAGE - 1))) / 8) as usize).min(left / 8);
        let st = bus.read_block(a, &mut words[..n]);
        if st != BUS_OK {
            return st;
        }
        for (k, w) in words[..n].iter().enumerate() {
            buf[done + 8 * k..done + 8 * k + 8].copy_from_slice(&w.to_be_bytes());
        }
        done += 8 * n;
    }
    BUS_OK
}

pub fn write_bytes(bus: &dyn BusDevice, addr: u64, data: &[u8]) -> u32 {
    const PAGE: u64 = 4096;
    let mut done = 0usize;
    let mut words = [0u64; (PAGE / 8) as usize];
    while done < data.len() {
        let a = addr.wrapping_add(done as u64);
        let left = data.len() - done;
        if a & 7 != 0 || left < 8 {
            let st = bus.write8(a, data[done]);
            if st != BUS_OK {
                return st;
            }
            done += 1;
            continue;
        }
        let n = (((PAGE - (a & (PAGE - 1))) / 8) as usize).min(left / 8);
        for (k, w) in words[..n].iter_mut().enumerate() {
            *w = u64::from_be_bytes(data[done + 8 * k..done + 8 * k + 8].try_into().unwrap());
        }
        let st = bus.write_block(a, &words[..n]);
        if st != BUS_OK {
            return st;
        }
        done += 8 * n;
    }
    BUS_OK
}
