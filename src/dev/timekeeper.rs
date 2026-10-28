//! M48T35-compatible system Timekeeper, with independent byte-wide storage.
//!
//! Ported from hw/rtc/m48t35.c in jamesbraid/qemu-sgi-origami.
// SPDX-License-Identifier: BSD-3-Clause

use crate::traits::{BusDevice, BusRead8, Device, Resettable, BUS_ERR, BUS_OK};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const SIZE: usize = 0x8000;
pub const CONTROL: usize = 0x7ff8;
const CENTURY: i64 = 36525 * 86400;
const MAGIC: &[u8; 8] = b"M48TREF1";
fn bcd(v: i64) -> u8 {
    ((v / 10) * 16 + v % 10) as u8
}
fn unbcd(v: u8) -> i64 {
    i64::from((v >> 4) * 10 + (v & 15))
}
fn month_days(y: i64, m: i64) -> i64 {
    match m {
        2 => 28 + i64::from(y % 4 == 0),
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}
#[derive(Clone, Copy)]
struct Calendar {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
}
impl Calendar {
    fn seconds(self) -> i64 {
        let year = (self.year + (self.month - 1).div_euclid(12)).rem_euclid(100);
        let month = (self.month - 1).rem_euclid(12) + 1;
        let mut days = year * 365 + (year + 3) / 4 + self.day - 1;
        for m in 1..month {
            days += month_days(year, m);
        }
        (days * 86400 + self.hour * 3600 + self.minute * 60 + self.second).rem_euclid(CENTURY)
    }
    fn from_seconds(t: i64) -> Self {
        let t = t.rem_euclid(CENTURY);
        let mut days = t / 86400;
        let mut year = 0;
        while days >= 365 + i64::from(year % 4 == 0) {
            days -= 365 + i64::from(year % 4 == 0);
            year += 1;
        }
        let mut month = 1;
        while days >= month_days(year, month) {
            days -= month_days(year, month);
            month += 1;
        }
        Self {
            year,
            month,
            day: days + 1,
            hour: t / 3600 % 24,
            minute: t / 60 % 60,
            second: t % 60,
        }
    }
}
fn saved_calendar(image: &[u8]) -> Option<Calendar> {
    if image.len() != SIZE {
        return None;
    }
    let values = [
        image[0x7ff9] & 0x7f,
        image[0x7ffa],
        image[0x7ffb],
        image[0x7ffd],
        image[0x7ffe],
        image[0x7fff],
    ];
    let limits = [(0, 59), (0, 59), (0, 23), (1, 31), (1, 12), (0, 99)];
    let valid = values
        .iter()
        .zip(limits)
        .all(|(&v, (lo, hi))| v & 15 <= 9 && v >> 4 <= 9 && (lo..=hi).contains(&unbcd(v)));
    if !valid {
        return None;
    }
    Some(Calendar {
        second: unbcd(values[0]),
        minute: unbcd(values[1]),
        hour: unbcd(values[2]),
        day: unbcd(values[3]),
        month: unbcd(values[4]),
        year: unbcd(values[5]),
    })
}
struct State {
    cells: Vec<u8>,
    offset: i64,
    stopped: Option<i64>,
    frozen: Option<i64>,
    staged: Option<(Calendar, bool)>,
    file: Option<PathBuf>,
    #[cfg(test)]
    extra: i64,
}
impl State {
    fn running(&self, host: i64) -> i64 {
        self.stopped
            .unwrap_or_else(|| (host + self.offset).rem_euclid(CENTURY))
    }
    fn calendar(&self, host: i64) -> Calendar {
        Calendar::from_seconds(self.frozen.unwrap_or_else(|| self.running(host)))
    }
}

/// Fuel's 1968-based clock. Ordinary storage starts blank; the guest owns it.
/// The lower candidate clock addresses are independent RAM, with no aliases.
pub struct Timekeeper {
    initial_epoch: i64,
    st: Mutex<State>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}
impl Timekeeper {
    pub fn fuel() -> Result<Self, String> {
        let host = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_secs() as i64;
        // Gregorian host date, used only to initialize the part's own calendar.
        let mut days = host / 86400;
        let mut year = 1970;
        loop {
            let n = 365 + i64::from(year % 4 == 0 && (year % 100 != 0 || year % 400 == 0));
            if days < n {
                break;
            }
            days -= n;
            year += 1;
        }
        let mut month = 1;
        while days >= month_days(year, month) {
            days -= month_days(year, month);
            month += 1;
        }
        let t = Self::fuel_at(
            year,
            month,
            days + 1,
            host / 3600 % 24,
            host / 60 % 60,
            host % 60,
        )?;
        Ok(t)
    }
    /// Open authoritative saved clock state before applying new-store year limits.
    pub fn fuel_with_nvram_file(path: &Path) -> Result<Self, String> {
        Self::backed(path, Self::fuel())
    }
    fn backed(path: &Path, initial: Result<Self, String>) -> Result<Self, String> {
        let saved = match std::fs::read(path) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.to_string()),
        };
        let clock = if saved_calendar(&saved).is_some() {
            // This seed is replaced by the authoritative image before exposure.
            Self::fuel_at(1968, 1, 1, 0, 0, 0)?
        } else {
            initial?
        };
        clock.set_nvram_file(path)?;
        Ok(clock)
    }
    /// A deterministic initial date; subsequent seconds follow monotonic host time.
    pub fn fuel_at(
        year: i64,
        month: i64,
        day: i64,
        hour: i64,
        minute: i64,
        second: i64,
    ) -> Result<Self, String> {
        if !(1968..=2067).contains(&year) {
            return Err("Timekeeper: initial year is outside 1968-2067".into());
        }
        if !(1..=12).contains(&month)
            || !(1..=month_days(year, month)).contains(&day)
            || !(0..24).contains(&hour)
            || !(0..60).contains(&minute)
            || !(0..60).contains(&second)
        {
            return Err("Timekeeper: invalid initial calendar".into());
        }
        let epoch = Calendar {
            year: year - 1968,
            month,
            day,
            hour,
            minute,
            second,
        }
        .seconds();
        let at = Instant::now();
        Ok(Self {
            initial_epoch: epoch,
            st: Mutex::new(State {
                cells: vec![0; SIZE],
                offset: epoch,
                stopped: None,
                frozen: None,
                staged: None,
                file: None,
                #[cfg(test)]
                extra: 0,
            }),
            clock: Arc::new(move || at.elapsed().as_secs() as i64),
        })
    }
    fn host(&self, st: &State) -> i64 {
        #[cfg(not(test))]
        let _ = st;
        (self.clock)() + {
            #[cfg(test)]
            {
                st.extra
            }
            #[cfg(not(test))]
            {
                0
            }
        }
    }
    #[cfg(test)]
    fn advance_for_test(&self, seconds: i64) {
        self.st.lock().extra += seconds;
    }

    fn image(st: &State, host: i64, saved_at: i64) -> (Vec<u8>, [u8; 16]) {
        let c = Calendar::from_seconds(st.running(host));
        let mut image = st.cells.clone();
        image[CONTROL + 1] = bcd(c.second) | if st.stopped.is_some() { 0x80 } else { 0 };
        image[CONTROL + 2] = bcd(c.minute);
        image[CONTROL + 3] = bcd(c.hour);
        image[CONTROL + 5] = bcd(c.day);
        image[CONTROL + 6] = bcd(c.month);
        image[CONTROL + 7] = bcd(c.year);
        let mut reference = [0; 16];
        reference[..8].copy_from_slice(MAGIC);
        reference[8..].copy_from_slice(&saved_at.to_le_bytes());
        (image, reference)
    }
    /// Export the exact raw image and companion clock reference, without changing RAM.
    pub fn persistence_image_at(&self, saved_at: i64) -> (Vec<u8>, [u8; 16]) {
        let st = self.st.lock();
        Self::image(&st, self.host(&st), saved_at)
    }
    /// Restore validated storage. Invalid/unwritten clock cells keep the initial clock.
    /// A missing/invalid reference adds no elapsed time, matching the donor policy.
    pub fn restore_image_at(
        &self,
        image: &[u8],
        reference: Option<&[u8]>,
        now: i64,
    ) -> Result<(), String> {
        if image.len() != SIZE {
            return Err(format!(
                "Timekeeper: backing image has {} bytes, expected {SIZE}",
                image.len()
            ));
        }
        if reference.is_some_and(|r| r.len() < 16) {
            return Err("Timekeeper: clock reference is shorter than 16 bytes".into());
        }
        let mut st = self.st.lock();
        st.cells.copy_from_slice(image);
        st.frozen = None;
        st.staged = None;
        st.stopped = None;
        if let Some(c) = saved_calendar(image) {
            let stopped = image[0x7ff9] & 0x80 != 0;
            let elapsed = reference.filter(|r| &r[..8] == MAGIC).map_or(0, |r| {
                now.saturating_sub(i64::from_le_bytes(r[8..16].try_into().unwrap()))
                    .max(0)
            });
            let epoch = (c.seconds()
                + if stopped {
                    0
                } else {
                    elapsed.rem_euclid(CENTURY)
                })
            .rem_euclid(CENTURY);
            st.offset = epoch - self.host(&st);
            if stopped {
                st.stopped = Some(epoch);
            }
        } else {
            st.offset = self.initial_epoch;
            log::warn!("Timekeeper: unwritten or invalid clock cells; following initial clock");
        }
        Ok(())
    }
    /// Bind a raw store and its `.clock` companion. Validate both before creating either.
    pub fn set_nvram_file(&self, path: &Path) -> Result<(), String> {
        let clock_path = PathBuf::from(format!("{}.clock", path.display()));
        let image = match std::fs::read(path) {
            Ok(v) => Some(v),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.to_string()),
        };
        if image.as_ref().is_some_and(|v| v.len() != SIZE) {
            return Err("Timekeeper: backing image must contain exactly 32768 bytes".into());
        }
        let reference = match std::fs::read(&clock_path) {
            Ok(v) => Some(v),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.to_string()),
        };
        if reference.as_ref().is_some_and(|v| v.len() < 16) {
            return Err("Timekeeper: clock reference is shorter than 16 bytes".into());
        }
        if let Some(image) = image.as_ref() {
            self.restore_image_at(image, reference.as_deref(), unix_now())?;
        }
        use std::io::Write;
        if image.is_none() {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .and_then(|mut f| f.write_all(&vec![0; SIZE]))
                .map_err(|e| e.to_string())?;
        }
        if reference.is_none() {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(clock_path)
                .and_then(|mut f| f.write_all(&[0; 16]))
                .map_err(|e| e.to_string())?;
        }
        self.st.lock().file = Some(path.to_path_buf());
        Ok(())
    }
    pub fn flush_nvram(&self) -> Result<(), String> {
        let st = self.st.lock();
        if let Some(path) = &st.file {
            let (image, reference) = Self::image(&st, self.host(&st), unix_now());
            std::fs::write(path, image).map_err(|e| e.to_string())?;
            use std::io::Write;
            std::fs::OpenOptions::new()
                .write(true)
                .open(format!("{}.clock", path.display()))
                .and_then(|mut f| f.write_all(&reference))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
impl BusDevice for Timekeeper {
    fn read8(&self, addr: u64) -> BusRead8 {
        let Ok(a) = usize::try_from(addr) else {
            return BusRead8::err();
        };
        if a >= SIZE {
            return BusRead8::err();
        }
        let st = self.st.lock();
        let c = st.calendar(self.host(&st));
        let data = match a {
            0x7ff9 => bcd(c.second) | if st.stopped.is_some() { 0x80 } else { 0 },
            0x7ffa => bcd(c.minute),
            0x7ffb => bcd(c.hour),
            0x7ffd => bcd(c.day),
            0x7ffe => bcd(c.month),
            0x7fff => bcd(c.year),
            _ => st.cells[a],
        };
        BusRead8 {
            status: BUS_OK,
            data,
        }
    }
    fn write8(&self, addr: u64, val: u8) -> u32 {
        let Ok(a) = usize::try_from(addr) else {
            return BUS_ERR;
        };
        if a >= SIZE {
            return BUS_ERR;
        }
        let mut st = self.st.lock();
        let host = self.host(&st);
        let old_control = st.cells[CONTROL];
        let transfer = a == CONTROL && st.cells[CONTROL] & 0x80 != 0 && val & 0x80 == 0;
        if a == CONTROL {
            let old = st.cells[a];
            if val & 0x40 != 0 && old & 0x40 == 0 {
                st.frozen = Some(st.running(host));
            } else if val & 0x40 == 0 {
                st.frozen = None;
            }
            if val & 0x80 != 0 && old & 0x80 == 0 {
                st.staged = Some((st.calendar(host), st.stopped.is_some()));
            } else if val & 0x80 == 0 && old & 0x80 != 0 {
                if let Some((c, stop)) = st.staged.take() {
                    let t = c.seconds();
                    st.offset = t - host;
                    st.stopped = if stop { Some(t) } else { None };
                }
            }
            st.cells[a] = val;
        } else if a > CONTROL {
            if a == 0x7ffc && st.staged.is_some() {
                st.cells[a] = val;
            } else if let Some((c, stop)) = &mut st.staged {
                match a {
                    0x7ff9 => {
                        c.second = unbcd(val & 0x7f);
                        *stop = val & 0x80 != 0;
                    }
                    0x7ffa => c.minute = unbcd(val),
                    0x7ffb => c.hour = unbcd(val),
                    0x7ffd => c.day = unbcd(val),
                    0x7ffe => c.month = unbcd(val),
                    0x7fff => c.year = unbcd(val),
                    _ => {}
                }
            } else {
                log::warn!("Timekeeper: clock write at {a:#x} without write enable");
            }
        } else {
            st.cells[a] = val;
        }
        if !transfer
            && (a < CONTROL
                || a == CONTROL && (old_control ^ val) & !0xc0 != 0
                || a == 0x7ffc && st.staged.is_some())
        {
            // Ordinary storage writes must not save or replace the clock cells.
            if let Some(path) = &st.file {
                use std::io::{Seek, SeekFrom, Write};
                let result =
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(path)
                        .and_then(|mut f| {
                            f.seek(SeekFrom::Start(a as u64))?;
                            f.write_all(&[st.cells[a]])
                        });
                if let Err(e) = result {
                    log::error!("Timekeeper: backing write failed: {e}");
                }
            }
        }
        drop(st);
        if transfer {
            if let Err(e) = self.flush_nvram() {
                log::error!("Timekeeper: clock save failed: {e}");
            }
        }
        BUS_OK
    }
}
impl Resettable for Timekeeper {
    fn power_on(&self) {
        let mut st = self.st.lock();
        st.frozen = None;
        st.staged = None;
        st.cells[CONTROL] &= !0xc0;
    }
}
impl Device for Timekeeper {
    fn start(&self) {}
    fn step(&self, _cycles: u64) {}
    fn is_running(&self) -> bool {
        false
    }
    fn get_clock(&self) -> u64 {
        0
    }
    fn stop(&self) {
        if let Err(e) = self.flush_nvram() {
            log::error!("Timekeeper: saving failed: {e}");
        }
    }
}
impl Drop for Timekeeper {
    fn drop(&mut self) {
        Device::stop(self);
    }
}
#[cfg(test)]
mod tests;
