//! The attach handles of a test, played by one object.

use super::*;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// The attach handles of a test: it records the packets, holds guest RAM and
/// counts the FIFO-drained signals.
pub(in crate::dev::kona) struct TestHost {
    pub packets: Mutex<Vec<InterruptPacket>>,
    pub ram: Mutex<Vec<u8>>,
    pub drained: AtomicU32,
    /// Addresses at and above this fail to read and write.
    pub limit: u64,
}

pub(in crate::dev::kona) const RAM: usize = 16 << 20;

impl TestHost {
    pub fn new() -> Arc<TestHost> {
        Arc::new(TestHost { packets: Mutex::new(Vec::new()), ram: Mutex::new(vec![0; RAM]), drained: AtomicU32::new(0), limit: RAM as u64 })
    }

    pub fn word(&self, addr: u64) -> u32 {
        let r = self.ram.lock();
        u32::from_be_bytes(r[addr as usize..addr as usize + 4].try_into().unwrap())
    }

    pub fn put(&self, addr: u64, bytes: &[u8]) {
        self.ram.lock()[addr as usize..addr as usize + bytes.len()].copy_from_slice(bytes);
    }

    pub fn put_words(&self, addr: u64, words: &[u32]) {
        for (i, w) in words.iter().enumerate() {
            self.put(addr + 4 * i as u64, &w.to_be_bytes());
        }
    }

    pub fn bytes(&self, addr: u64, len: usize) -> Vec<u8> {
        self.ram.lock()[addr as usize..addr as usize + len].to_vec()
    }

    pub fn sent(&self) -> Vec<InterruptPacket> {
        self.packets.lock().clone()
    }
}

impl PacketSender for TestHost {
    fn send(&self, p: InterruptPacket) {
        self.packets.lock().push(p);
    }
}

impl HostDma for TestHost {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        if addr + buf.len() as u64 > self.limit {
            return false;
        }
        buf.copy_from_slice(&self.ram.lock()[addr as usize..addr as usize + buf.len()]);
        true
    }

    fn write(&self, addr: u64, data: &[u8]) -> bool {
        if addr + data.len() as u64 > self.limit {
            return false;
        }
        self.ram.lock()[addr as usize..addr as usize + data.len()].copy_from_slice(data);
        true
    }
}

impl FifoLow for TestHost {
    fn drained(&self) {
        self.drained.fetch_add(1, Ordering::SeqCst);
    }
}

pub(in crate::dev::kona) fn attach(h: &Arc<TestHost>) -> KonaAttach {
    KonaAttach { packets: h.clone(), dma: h.clone(), fifo_low: h.clone() }
}

