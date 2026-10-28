//! Fuel's L1 system controller and processor-side UART.
//!
//! Ported from hw/misc/sgi-l1.c in jamesbraid/qemu-sgi-origami.
//! The synthetic identities and unmeasured power timing retain that model's
//! approximations. Remote routing and controller diagnostics are unsupported.
// SPDX-License-Identifier: BSD-3-Clause

use crate::dev::z85c30::{null_serial_backend, SerialBackend};
use crate::irq::IrqLine;
use crate::traits::{BusDevice, BusRead64, Device, Resettable, BUS_ERR, BUS_OK};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
mod model;
use model::State;
pub use model::{L1Config, L1Event};

fn fcs(mut crc: u16, byte: u8) -> u16 {
    crc ^= byte as u16;
    for _ in 0..8 {
        crc = (crc >> 1) ^ if crc & 1 != 0 { 0x8408 } else { 0 };
    }
    crc
}
fn encode(payload: &[u8]) -> Vec<u8> {
    let crc = !payload.iter().fold(0xffff, |v, b| fcs(v, *b));
    let mut out = vec![0x7e];
    for b in payload.iter().copied().chain([crc as u8, (crc >> 8) as u8]) {
        if matches!(b, 0x7e | 0x7d | 0x0d) {
            out.extend_from_slice(&[0x7d, b ^ 0x20]);
        } else {
            out.push(b);
        }
    }
    out.push(0x7e);
    out
}
type EventHandler = Arc<dyn Fn(L1Event) + Send + Sync>;

/// The internal L1 connection is independent of the IOC3's external serial ports.
/// Register offsets are relative to the 0x40-byte UART window. Only aligned
/// 64-bit accesses are decoded; register values occupy the low eight bits.
pub struct L1 {
    state: Arc<Mutex<State>>,
    backend: Arc<Mutex<Arc<dyn SerialBackend>>>,
    irq: Arc<Mutex<IrqLine>>,
    handler: Arc<Mutex<Option<EventHandler>>>,
    running: Arc<AtomicBool>,
    clock: Arc<AtomicU64>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    wake: Arc<(Mutex<()>, parking_lot::Condvar)>,
}
impl L1 {
    pub fn new(config: L1Config) -> Result<Self, String> {
        if config.rack > 0x3ff || config.bay > 0x3f || config.node_position > 3 {
            return Err("L1 rack, bay or node position out of range".into());
        }
        if config.mac == [0; 6] || config.mac[0] & 1 != 0 {
            return Err("L1 MAC must be nonzero unicast".into());
        }
        if config
            .brick_serial
            .as_ref()
            .is_some_and(|s| s.len() > 37 || s.as_bytes().contains(&0))
        {
            return Err("L1 brick serial must fit a 37-byte text field".into());
        }
        Ok(Self {
            state: Arc::new(Mutex::new(State::new(config))),
            backend: Arc::new(Mutex::new(null_serial_backend())),
            irq: Arc::new(Mutex::new(IrqLine::unconnected())),
            handler: Arc::new(Mutex::new(None)),
            running: Arc::new(AtomicBool::new(false)),
            clock: Arc::new(AtomicU64::new(0)),
            worker: Mutex::new(None),
            wake: Arc::new((Mutex::new(()), parking_lot::Condvar::new())),
        })
    }
    pub fn set_serial_backend(&self, backend: Arc<dyn SerialBackend>) {
        *self.backend.lock() = backend;
    }
    pub fn set_irq(&self, line: IrqLine) {
        *self.irq.lock() = line;
        self.flush();
    }
    /// Power/reset requests reach the machine only after the reply is queued
    /// and after the device state lock is released.
    pub fn set_event_handler(&self, handler: EventHandler) {
        *self.handler.lock() = Some(handler);
    }
    pub fn node_powered(&self) -> bool {
        self.state.lock().powered
    }
    pub fn station_mac(&self) -> [u8; 6] {
        self.state.lock().config.mac
    }
    pub fn display_line(&self) -> String {
        self.state.lock().display.clone()
    }
    pub fn record(&self, component: usize, area: u32) -> Option<Vec<u8>> {
        let index = match area {
            1 => 0,
            2 => 1,
            4 => 2,
            _ => return None,
        };
        self.state.lock().records.get(component).and_then(|r| {
            if r[index].is_empty() {
                None
            } else {
                Some(r[index].clone())
            }
        })
    }
    /// Install a raw area before execution. It replaces only the named area;
    /// station identity remains the configured MAC, as in the Fuel donor.
    pub fn set_record(&self, component: usize, area: u32, bytes: Vec<u8>) -> Result<(), String> {
        let index = match area {
            1 => 0,
            2 => 1,
            4 => 2,
            _ => return Err("unsupported L1 area".into()),
        };
        if component >= 16 || bytes.len() > 65535 {
            return Err("L1 record out of range".into());
        }
        self.state.lock().records[component][index] = bytes;
        Ok(())
    }
    pub fn send_event(&self, code: u32, number: u32, text: &str) {
        self.state.lock().event(code, number, text);
        self.flush();
    }
    /// Deterministic input path shared with the polled SerialBackend.
    pub fn receive_console(&self, bytes: &[u8]) {
        for &b in bytes {
            self.state.lock().console_input(b);
            self.flush();
        }
    }
    pub fn poll_serial(&self) {
        let backend = self.backend.lock().clone();
        for _ in 0..256 {
            if self.state.lock().queue.len() > 4088 {
                break;
            }
            match backend.recv_byte() {
                Ok(b) => self.receive_console(&[b]),
                Err(e) => {
                    if e.kind() != std::io::ErrorKind::WouldBlock {
                        log::debug!("L1 serial input: {e}");
                    }
                    break;
                }
            }
        }
    }
    fn flush(&self) {
        let (level, output, actions) = {
            let mut s = self.state.lock();
            (
                s.cause() != 1,
                std::mem::take(&mut s.output),
                std::mem::take(&mut s.actions),
            )
        };
        let irq = self.irq.lock().clone();
        irq.set(level);
        let backend = self.backend.lock().clone();
        for b in output {
            backend.send_byte(b);
        }
        let handler = self.handler.lock().clone();
        for action in actions {
            if let Some(handler) = &handler {
                handler(action);
            } else {
                log::warn!("unconnected L1 action {action:?}");
            }
        }
    }
}
impl BusDevice for L1 {
    fn read64(&self, addr: u64) -> BusRead64 {
        if addr >= 0x40 || addr & 7 != 0 {
            return BusRead64::err();
        }
        let value = {
            let mut s = self.state.lock();
            match addr {
                0 => {
                    if s.regs[3] & 0x80 != 0 {
                        s.divisor[0]
                    } else {
                        s.queue.pop_front().unwrap_or(0)
                    }
                }
                8 => {
                    if s.regs[3] & 0x80 != 0 {
                        s.divisor[1]
                    } else {
                        s.regs[1]
                    }
                }
                0x10 => {
                    let cause = s.cause();
                    if cause == 2 {
                        s.thre = false;
                    }
                    cause
                }
                0x18 => s.regs[3],
                0x20 => s.regs[4],
                0x28 => 0x60 | u8::from(!s.queue.is_empty()),
                0x30 => 0x10,
                0x38 => s.regs[7],
                _ => unreachable!(),
            }
        };
        self.flush();
        BusRead64::ok(value as u64)
    }
    fn write64(&self, addr: u64, value: u64) -> u32 {
        if addr >= 0x40 || addr & 7 != 0 {
            return BUS_ERR;
        }
        {
            let mut s = self.state.lock();
            let v = value as u8;
            match addr {
                0 => {
                    if s.regs[3] & 0x80 != 0 {
                        s.divisor[0] = v;
                    } else {
                        s.thre = false;
                        s.parse(v);
                        s.thre = s.regs[1] & 2 != 0;
                    }
                }
                8 => {
                    if s.regs[3] & 0x80 != 0 {
                        s.divisor[1] = v;
                    } else {
                        if v & 2 != 0 && s.regs[1] & 2 == 0 {
                            s.thre = true;
                        }
                        s.regs[1] = v;
                        if v & 0xfc != 0 {
                            log::warn!("L1 unsupported UART interrupt enables {v:#x}");
                        }
                    }
                }
                0x10 => {
                    if v & 2 != 0 {
                        s.queue.clear();
                    }
                }
                0x18 => s.regs[3] = v,
                0x20 => s.regs[4] = v,
                0x38 => s.regs[7] = v,
                _ => log::warn!("L1 write to read-only UART register {addr:#x}"),
            }
        }
        self.flush();
        BUS_OK
    }
}
impl Resettable for L1 {
    fn power_on(&self) {
        let mut s = self.state.lock();
        let config = s.config.clone();
        let records = std::mem::replace(
            &mut s.records,
            std::array::from_fn(|_| std::array::from_fn(|_| Vec::new())),
        );
        *s = State::new(config);
        s.records = records;
        drop(s);
        self.clock.store(0, Ordering::Relaxed);
        self.flush();
    }
}
impl Device for L1 {
    fn start(&self) {
        let mut worker = self.worker.lock();
        if worker.is_some() {
            return;
        }
        self.running.store(true, Ordering::Release);
        let device = Self {
            state: self.state.clone(),
            backend: self.backend.clone(),
            irq: self.irq.clone(),
            handler: self.handler.clone(),
            running: self.running.clone(),
            clock: self.clock.clone(),
            worker: Mutex::new(None),
            wake: self.wake.clone(),
        };
        *worker = Some(
            std::thread::Builder::new()
                .name("l1-serial".into())
                .spawn(move || {
                    while device.is_running() {
                        device.poll_serial();
                        let mut guard = device.wake.0.lock();
                        if device.is_running() {
                            device
                                .wake
                                .1
                                .wait_for(&mut guard, std::time::Duration::from_millis(2));
                        }
                    }
                })
                .expect("start L1 serial worker"),
        );
    }
    fn stop(&self) {
        let mut slot = self.worker.lock();
        {
            let _guard = self.wake.0.lock();
            self.running.store(false, Ordering::Release);
            self.wake.1.notify_all();
        }
        let worker = slot.take();
        if let Some(worker) = worker {
            if worker.thread().id() != std::thread::current().id() {
                let _ = worker.join();
            }
        }
    }
    fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }
    fn get_clock(&self) -> u64 {
        self.clock.load(Ordering::Relaxed)
    }
    fn step(&self, cycles: u64) {
        if self.is_running() {
            self.clock.fetch_add(cycles, Ordering::Relaxed);
        }
    }
}
impl Drop for L1 {
    fn drop(&mut self) {
        self.stop();
    }
}
#[cfg(test)]
mod tests;
