//! The QLogic ISP12160: two SCSI channels behind one PCI function.
//!
//! Both channels use the ISP1040 command engine, with one request/response
//! queue pair and one interrupt. Commands execute serially on the device's
//! joined worker thread. The downloaded controller program is retained but
//! not executed.
//!
//! Ported from hw/scsi/qlogic-isp.c and hw/scsi/qlogic-isp.h
//! in jamesbraid/qemu-sgi-origami.

use std::sync::Arc;
use crate::dev::isp1040::{Core, Isp1040};
use crate::irq::IrqLine;
use crate::pci::{PciBusDevice, PciConfig, PciDma};
use crate::scsi::ScsiDevice;
use crate::traits::{Device, Resettable, Saveable};

pub struct Isp12160(Isp1040);

impl Default for Isp12160 {
    fn default() -> Self { Self::new() }
}

impl Isp12160 {
    pub fn new() -> Self { Self(Isp1040::new_12160()) }

    pub fn register_locks(&self) { self.0.register_locks(); }

    /// Lend a channel for the shared SCSI monitor and disc changer interfaces.
    pub fn channel(self: &Arc<Self>, channel: u8) -> Option<Channel> {
        (channel < 2).then(|| Channel { adapter: self.clone(), channel })
    }
}

impl std::ops::Deref for Isp12160 {
    type Target = Core;
    fn deref(&self) -> &Core { &self.0 }
}

/// A channel handle keeps its PCI function alive without creating another adapter.
pub struct Channel {
    adapter: Arc<Isp12160>,
    channel: u8,
}

impl crate::scsi_monitor::ScsiTargets for Channel {
    fn with_targets<R>(&self, f: impl FnOnce(&mut [Option<ScsiDevice>]) -> R) -> R {
        let mut targets = self.adapter.channel_targets(self.channel).lock();
        f(&mut targets[..])
    }
}

impl Device for Isp12160 {
    fn step(&self, cycles: u64) { self.0.step(cycles) }
    fn start(&self) { self.0.start() }
    fn stop(&self) { self.0.stop() }
    fn is_running(&self) -> bool { self.0.is_running() }
    fn get_clock(&self) -> u64 { self.0.get_clock() }
}

impl Resettable for Isp12160 {
    fn power_on(&self) { self.0.reset(); self.0.config().reset(); }
}

impl Saveable for Isp12160 {
    fn save_state(&self) -> toml::Value { self.0.save_state() }
    fn load_state(&self, v: &toml::Value) -> Result<(), String> { self.0.load_state(v) }
}

impl PciBusDevice for Isp12160 {
    fn config(&self) -> &PciConfig { self.0.config() }
    fn bar_read(&self, bar: usize, off: u32, width: usize) -> u32 { self.0.bar_read(bar, off, width) }
    fn bar_write(&self, bar: usize, off: u32, val: u32, width: usize) { self.0.bar_write(bar, off, val, width) }
    fn connect_interrupt(&self, line: IrqLine) { self.0.connect_interrupt(line) }
    fn connect_dma(&self, dma: Arc<dyn PciDma>) { self.0.connect_dma(dma) }
}
