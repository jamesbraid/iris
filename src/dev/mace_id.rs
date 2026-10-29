//! MACE's miscellaneous control register at 0x1f310008: the flash write
//! enable, the identity line to the board's DS2502 and the two front LEDs.
//! The line is two bits, not a slot register: the master clears RELEASE to
//! pull it low and sets it to let go, and SAMPLE reads it back. What a pulse
//! means depends on how long the line stayed low, so the device times it on
//! the guest clock and hands each finished reset or bit slot to the shared
//! [`w1::Bus`].
//!
//! Ported from hw/mips/sgi/mace-id.c in jamesbraid/qemu-sgi-origami.

use std::sync::Mutex;

use crate::dev::w1::{self, sgi};
use crate::snapshot::{get_field, toml_u64};

pub const FLASH_ENABLE: u8 = 1;
const PASSWORD_CLEAR: u8 = 1 << 1;
/// The master lets the line float high.
pub const RELEASE: u8 = 1 << 2;
/// The line as it reads back.
pub const SAMPLE: u8 = 1 << 3;
const RED_LED: u8 = 1 << 4;
const GREEN_LED: u8 = 1 << 5;
const WRITABLE: u8 = FLASH_ENABLE | RELEASE | RED_LED | GREEN_LED;

// DS2502 datasheet timing, in microseconds of guest time.
const RESET_MIN: i64 = 480;
const RESET_MAX: i64 = 960;
const PRESENCE_DELAY: i64 = 30;
const PRESENCE_END: i64 = 150;
/// The responder takes wider lows than the part would for a one.
const DIAG_ONE_MAX: i64 = 24;
const SLOT: i64 = 60;
const WRITE_ZERO: i64 = 60;
const SLOT_MAX: i64 = 120;

/// The station address the board carries when none is configured.
pub const DEFAULT_MAC: [u8; 6] = [0x08, 0x00, 0x69, 0x12, 0x34, 0x56];

struct State {
    bus: w1::Bus,
    low_start: i64,
    startup_low_start: i64,
    high_start: i64,
    presence: (i64, i64),
    sample: (i64, i64),
    released: bool,
    flash_write_enabled: bool,
    led_control: u8,
    high_seen: bool,
    reset_ready: bool,
    startup_pending: bool,
    startup_low_valid: bool,
    session: bool,
    slot_pull: bool,
}

pub struct MaceId {
    st: Mutex<State>,
}

impl State {
    fn reset(&mut self) {
        // Reset changes the drive outputs, not the connected board input.
        self.presence = (0, 0);
        self.sample = (0, 0);
        self.slot_pull = false;
        self.released = false;
        self.flash_write_enabled = false;
        self.led_control = GREEN_LED;
        self.high_seen = false;
        self.reset_ready = false;
        self.startup_pending = true;
        self.startup_low_valid = false;
        self.session = false;
        self.low_start = 0;
        self.startup_low_start = 0;
        self.high_start = 0;
        self.bus.reset();
    }

    fn clear_windows(&mut self) {
        self.presence = (0, 0);
        self.sample = (0, 0);
        self.slot_pull = false;
    }

    fn presence_at(&self, now: i64) -> bool {
        self.session && now >= self.presence.0 && now < self.presence.1
    }

    fn value(&self, now: i64) -> u8 {
        let sample_pull = self.slot_pull && now >= self.sample.0 && now < self.sample.1;
        let level = self.released && !self.presence_at(now) && !sample_pull;
        self.led_control
            | if self.flash_write_enabled { FLASH_ENABLE } else { 0 }
            | if self.released { RELEASE } else { 0 }
            | if level { SAMPLE } else { 0 }
    }

    fn reset_pulse(&mut self, now: i64, present: bool) {
        self.clear_windows();
        self.session = false;
        self.bus.reset();
        if present {
            self.session = true;
            self.presence = (now + PRESENCE_DELAY, now + PRESENCE_END);
        }
    }

    fn write(&mut self, now: i64, value: u8) {
        let release = value & RELEASE != 0;
        self.led_control = value & (RED_LED | GREEN_LED);
        self.flash_write_enabled = value & FLASH_ENABLE != 0;
        if !release && self.startup_pending && !self.startup_low_valid {
            self.startup_low_valid = true;
            self.startup_low_start = now;
        }
        if release == self.released {
            return;
        }
        self.released = release;
        if !release {
            self.reset_ready = self.high_seen && now - self.high_start >= RESET_MIN;
            self.low_start = now;
            return;
        }
        self.high_start = now;
        if self.startup_pending {
            let low = if self.startup_low_valid { now - self.startup_low_start } else { -1 };
            self.startup_pending = false;
            self.startup_low_valid = false;
            self.high_seen = true;
            if low >= RESET_MIN {
                self.reset_pulse(now, true);
            }
            return;
        }
        if !self.high_seen {
            self.high_seen = true;
            return;
        }
        let low = now - self.low_start;
        if low >= RESET_MIN {
            self.reset_pulse(now, low < RESET_MAX && self.reset_ready);
            return;
        }
        if !self.session || now < self.presence.1 {
            return;
        }
        if (WRITE_ZERO..SLOT_MAX).contains(&low) {
            self.bus.slot(false);
        } else if low < DIAG_ONE_MAX {
            // A one is also a read slot. The clock ticks in whole
            // microseconds, so a pulse can last none; the model's lower
            // bound of one is dropped.
            self.slot_pull = !self.bus.slot(true);
            self.sample = (now, self.low_start + SLOT);
        }
        // Anything between is a pulse the part has no slot for: ignored.
    }
}

impl MaceId {
    /// The board's Ethernet-address chip for station address `mac`.
    pub fn new(mac: [u8; 6]) -> Self {
        let mut bus = w1::Bus::new();
        bus.add(Box::new(sgi::o2_ethernet_address_part(mac)));
        let mut st = State {
            bus,
            low_start: 0,
            startup_low_start: 0,
            high_start: 0,
            presence: (0, 0),
            sample: (0, 0),
            released: false,
            flash_write_enabled: false,
            led_control: 0,
            high_seen: false,
            reset_ready: false,
            startup_pending: false,
            startup_low_valid: false,
            session: false,
            slot_pull: false,
        };
        st.reset();
        Self { st: Mutex::new(st) }
    }

    /// The register at guest time `now` (microseconds). The password-clear
    /// input is not wired, so bit 1 reads zero.
    pub fn read(&self, now: u64) -> u8 {
        self.st.lock().unwrap().value(now as i64) & !PASSWORD_CLEAR
    }

    pub fn write(&self, now: u64, value: u8) {
        self.st.lock().unwrap().write(now as i64, value & WRITABLE);
    }
}

impl crate::traits::Device for MaceId {
    fn step(&self, _cycles: u64) {}
    fn stop(&self) {}
    fn start(&self) {}
    fn is_running(&self) -> bool {
        true
    }
    fn get_clock(&self) -> u64 {
        0
    }

    fn register_commands(&self) -> Vec<(String, String)> {
        vec![("onewire".to_string(), "MACE identity line: onewire status".to_string())]
    }

    fn execute_command(&self, cmd: &str, _args: &[&str], mut w: Box<dyn std::io::Write + Send>) -> Result<(), String> {
        use std::io::Write;
        if cmd != "onewire" {
            return Err("Command not found".to_string());
        }
        let st = self.st.lock().unwrap();
        let rom: String = st.bus.roms()[0].iter().map(|b| format!("{b:02x}")).collect();
        writeln!(w, "ID chip ROM {rom}, register {:#04x}", st.value(0)).ok();
        Ok(())
    }
}

/// The register's state only, as the QEMU model keeps it. The part comes
/// from the configured MAC, and a restored guest is past the PROM's start-up
/// handshake.
impl crate::traits::Saveable for MaceId {
    fn save_state(&self) -> toml::Value {
        let st = self.st.lock().unwrap();
        let mut t = toml::map::Map::new();
        t.insert("register".into(), toml::Value::Integer((st.value(0) & !SAMPLE) as i64));
        toml::Value::Table(t)
    }

    fn load_state(&self, v: &toml::Value) -> Result<(), String> {
        let reg = get_field(v, "register").and_then(toml_u64).unwrap_or(0) as u8;
        let mut st = self.st.lock().unwrap();
        st.reset();
        st.startup_pending = false;
        st.high_seen = true;
        st.released = reg & RELEASE != 0;
        st.flash_write_enabled = reg & FLASH_ENABLE != 0;
        st.led_control = reg & (RED_LED | GREEN_LED);
        Ok(())
    }
}

impl crate::traits::Resettable for MaceId {
    fn power_on(&self) {
        self.st.lock().unwrap().reset();
    }
}
