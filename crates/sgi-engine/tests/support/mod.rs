//! What the tests share: a small guest RAM and the mock built over it.
#![allow(dead_code)]

use std::sync::Arc;

use parking_lot::Mutex;
use sgi_engine::mock::{Mock, MockConfig};
use sgi_engine::{HostMem, Lent, MemFault, Swap};

/// Guest RAM below `RAM`; everything above is not RAM.
pub const RAM: u64 = 0x1000;

#[derive(Clone, Default)]
pub struct Ram(pub Arc<Mutex<Vec<u8>>>);

impl Ram {
    pub fn new() -> Ram {
        Ram(Arc::new(Mutex::new(vec![0; RAM as usize])))
    }

    pub fn word(&self, addr: u64) -> u64 {
        u64::from_be_bytes(self.0.lock()[addr as usize..][..8].try_into().unwrap())
    }

    pub fn put(&self, addr: u64, v: u64) {
        self.0.lock()[addr as usize..][..8].copy_from_slice(&v.to_be_bytes());
    }
}

impl HostMem for Ram {
    fn ram_read(&self, addr: u64, out: &mut [u8], _: Swap) -> Result<(), MemFault> {
        let m = self.0.lock();
        let a = addr as usize;
        out.copy_from_slice(m.get(a..a + out.len()).ok_or(MemFault)?);
        Ok(())
    }

    fn ram_write(&self, addr: u64, data: &[u8], _: Swap) -> Result<(), MemFault> {
        let mut m = self.0.lock();
        let a = addr as usize;
        m.get_mut(a..a + data.len()).ok_or(MemFault)?.copy_from_slice(data);
        Ok(())
    }
}

/// 64 KiB of lent memory, 8-aligned, that lives as long as the test.
pub struct LentBuf(Vec<u64>);

impl LentBuf {
    pub fn new() -> LentBuf {
        LentBuf(vec![0; 8192])
    }

    pub fn lent(&mut self) -> Lent {
        // SAFETY: the buffer is 8-aligned and outlives the engine in every test.
        unsafe { Lent::new(self.0.as_mut_ptr().cast(), self.0.len() * 8) }.unwrap()
    }
}

pub fn mock(cfg: MockConfig) -> (Mock<Ram>, Ram) {
    let ram = Ram::new();
    (Mock::new(cfg, ram.clone(), None), ram)
}
