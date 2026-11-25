//! A shell's view of the engine for the tests: stores that retry on `Busy` and reads that drive the
//! engine while it answers `Busy`, as a bus that cannot retry does.

#![allow(dead_code)]

use sgi_engine::shell::drain_stalls;
use sgi_engine::{BoardEngine, HostMem, Idle, MemFault, Read, Swap, Write};
use sgi_gr2::dev::gr2::engine::{Config, Gr2Engine};
use sgi_gr2::dev::gr2::{hq2, Gr2Probe, Gr2Variant};

pub struct Ram;

impl HostMem for Ram {
    fn ram_read(&self, _: u64, out: &mut [u8], _: Swap) -> Result<(), MemFault> {
        out.fill(0);
        Ok(())
    }

    fn ram_write(&self, _: u64, _: &[u8], _: Swap) -> Result<(), MemFault> {
        Ok(())
    }
}

pub struct Rig {
    pub e: Gr2Engine<Ram>,
    pub now: u64,
}

impl Rig {
    pub fn new(variant: Gr2Variant) -> Rig {
        Rig::with_probe(variant.probe())
    }

    pub fn with_probe(probe: Gr2Probe) -> Rig {
        let mut config = Config::new(probe);
        config.jit = 0;
        Rig { e: Gr2Engine::new(config, Ram, None).unwrap(), now: 0 }
    }

    pub fn w32(&mut self, off: u32, val: u32) {
        loop {
            match self.e.write(off, 32, val as u64) {
                Write::Done => return,
                Write::Busy => {
                    drain_stalls(&mut self.e, &mut self.now, 1_000_000, 1000);
                }
                Write::Fault => panic!("write {off:#x}: {:?}", self.e.fault()),
            }
        }
    }

    pub fn r32(&mut self, off: u32) -> u32 {
        loop {
            match self.e.read(off, 32) {
                Read::Value(v) => return v as u32,
                Read::Busy => {
                    drain_stalls(&mut self.e, &mut self.now, 1_000_000, 1000);
                }
                Read::Fault => panic!("read {off:#x}: {:?}", self.e.fault()),
            }
        }
    }

    pub fn w8(&mut self, off: u32, val: u8) {
        assert_eq!(self.e.write(off, 8, val as u64), Write::Done);
    }

    /// A command token into the HQ2 FIFO window.
    pub fn cmd(&mut self, index: u32, val: u32) {
        self.w32(0x40000 + index * 4, val);
    }

    pub fn data(&mut self, val: u32) {
        self.cmd(hq2::PUC_DATA, val);
    }

    pub fn idle(&mut self) {
        assert_eq!(drain_stalls(&mut self.e, &mut self.now, 1_000_000, 10_000), Idle::Yes);
    }

    /// The PROM's textport setup prologue plus the kernel's initial clear.
    pub fn tp_init(&mut self) {
        self.w32(0x6a070, 0);
        self.w32(0x6a078, 0);
        self.cmd(hq2::PUC_INIT, 1);
        self.cmd(hq2::PUC_COLOR, 0);
        self.cmd(hq2::PUC_RECTI2D, 0);
        for v in [0, 1279, 1023] {
            self.data(v);
        }
    }

    /// DID 0 is 8-bit colour index through a ramp CLUT, the DACs are the identity.
    pub fn program_display(&mut self) {
        self.w32(0x6c1a0 + 0x10, 0);
        self.w32(0x6c1a0 + 0x14, 0);
        self.w32(0x6c1a0 + 0x04, 0x8200_0000);
        self.w32(0x6c1a0 + 0x14, 0x10);
        for i in 0..256u32 {
            self.w32(0x6c1a0 + 0x10, i);
            for c in [(i * 3) as u8, (255 - i) as u8, (i * 7) as u8] {
                self.w8(0x6c1a0 + 0x08, c);
            }
        }
        for dac in [0x6c0a0u32, 0x6c0c0, 0x6c0e0] {
            self.w32(dac, 0);
            for i in 0..256 {
                self.w32(dac + 4, i);
            }
            self.w32(dac, 4);
            self.w32(dac + 8, 0xff);
        }
    }

    /// Fill a rectangle of one colour index through the text-port commands.
    pub fn rect(&mut self, color: u32, x0: u32, y0: u32, x1: u32, y1: u32) {
        self.cmd(hq2::PUC_COLOR, color);
        self.cmd(hq2::PUC_RECTI2D, x0);
        for v in [y0, x1, y1] {
            self.data(v);
        }
    }
}
