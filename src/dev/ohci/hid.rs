//! Basic boot-subclass HID functions, using HID 1.11 Appendix B layouts.
//! Host input is expressed in HID usages; no host keycode mapping is assumed.
use super::*;
use std::collections::VecDeque;

const KEYBOARD_REPORT: &[u8] = &[
    0x05, 1, 0x09, 6, 0xa1, 1, 0x05, 7, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0, 0x25, 1, 0x75, 1, 0x95, 8,
    0x81, 2, 0x95, 1, 0x75, 8, 0x81, 1, 0x95, 5, 0x75, 1, 0x05, 8, 0x19, 1, 0x29, 5, 0x91, 2, 0x95,
    1, 0x75, 3, 0x91, 1, 0x95, 6, 0x75, 8, 0x15, 0, 0x25, 0x65, 0x05, 7, 0x19, 0, 0x29, 0x65, 0x81,
    0, 0xc0,
];
const MOUSE_REPORT: &[u8] = &[
    0x05, 1, 0x09, 2, 0xa1, 1, 0x09, 1, 0xa1, 0, 0x05, 9, 0x19, 1, 0x29, 3, 0x15, 0, 0x25, 1, 0x95,
    3, 0x75, 1, 0x81, 2, 0x95, 1, 0x75, 5, 0x81, 1, 0x05, 1, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81,
    0x25, 0x7f, 0x75, 8, 0x95, 2, 0x81, 6, 0xc0, 0xc0,
];
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Keyboard,
    Mouse,
}
struct Control {
    setup: [u8; 8],
    response: Vec<u8>,
    offset: usize,
    outgoing: Vec<u8>,
    expected: bool,
    previous: Option<(Pid, bool, Vec<u8>)>,
    stalled: bool,
}
struct HidState {
    address: u8,
    configured: bool,
    protocol: u8,
    idle: u8,
    leds: u8,
    now: u64,
    due: u64,
    report: Vec<u8>,
    changes: VecDeque<Vec<u8>>,
    buttons: u8,
    dx: i32,
    dy: i32,
    last: Option<Vec<u8>>,
    expected: bool,
    control: Option<Control>,
}
impl HidState {
    fn new(kind: Kind) -> Self {
        Self {
            address: 0,
            configured: false,
            protocol: 1,
            idle: if kind == Kind::Keyboard { 125 } else { 0 },
            leds: 0,
            now: 0,
            due: 500,
            report: vec![0; if kind == Kind::Keyboard { 8 } else { 3 }],
            changes: VecDeque::new(),
            buttons: 0,
            dx: 0,
            dy: 0,
            last: None,
            expected: false,
            control: None,
        }
    }
}
pub struct HidFunction {
    kind: Kind,
    state: Mutex<HidState>,
}
impl HidFunction {
    pub fn keyboard() -> Arc<Self> {
        Arc::new(Self {
            kind: Kind::Keyboard,
            state: Mutex::new(HidState::new(Kind::Keyboard)),
        })
    }
    pub fn mouse() -> Arc<Self> {
        Arc::new(Self {
            kind: Kind::Mouse,
            state: Mutex::new(HidState::new(Kind::Mouse)),
        })
    }
    pub fn inject_keyboard(&self, modifiers: u8, usages: &[u8]) -> Result<(), String> {
        if self.kind != Kind::Keyboard {
            return Err("not a keyboard".into());
        }
        if usages
            .iter()
            .any(|x| *x > 0x65 || (*x >= 0xe0 && *x <= 0xe7))
        {
            return Err("usage outside selected keyboard report".into());
        }
        let mut keys = usages.to_vec();
        keys.sort_unstable();
        keys.dedup();
        keys.retain(|x| *x != 0);
        let mut report = vec![modifiers, 0, 0, 0, 0, 0, 0, 0];
        if keys.len() > 6 {
            report[2..].fill(1)
        } else {
            report[2..2 + keys.len()].copy_from_slice(&keys)
        }
        let mut s = self.state.lock();
        if report != s.report {
            s.report = report.clone();
            s.changes.push_back(report);
        }
        Ok(())
    }
    pub fn inject_mouse(&self, buttons: u8, dx: i32, dy: i32) -> Result<(), String> {
        if self.kind != Kind::Mouse {
            return Err("not a mouse".into());
        }
        if buttons & !7 != 0 {
            return Err("three mouse buttons supported".into());
        }
        let mut s = self.state.lock();
        s.buttons = buttons;
        s.dx = s.dx.saturating_add(dx);
        s.dy = s.dy.saturating_add(dy);
        s.report = vec![buttons, 0, 0];
        Ok(())
    }
    pub fn leds(&self) -> u8 {
        self.state.lock().leds
    }
    fn report_descriptor(&self) -> &'static [u8] {
        if self.kind == Kind::Keyboard {
            KEYBOARD_REPORT
        } else {
            MOUSE_REPORT
        }
    }
    fn hid_descriptor(&self) -> Vec<u8> {
        let n = self.report_descriptor().len();
        vec![9, 0x21, 0x11, 1, 0, 1, 0x22, n as u8, (n >> 8) as u8]
    }
    fn descriptor(&self, kind: u8, index: u8) -> Option<Vec<u8>> {
        if index != 0 {
            return None;
        }
        match kind {
            // These are virtual product identities, not an assertion about a physical keyboard.
            1 => Some(vec![
                18, 1, 0x10, 1, 0, 0, 0, 8, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1,
            ]),
            2 => {
                let mut b = vec![
                    9,
                    2,
                    34,
                    0,
                    1,
                    1,
                    0,
                    0x80,
                    25,
                    9,
                    4,
                    0,
                    0,
                    1,
                    3,
                    1,
                    if self.kind == Kind::Keyboard { 1 } else { 2 },
                    0,
                ];
                b.extend(self.hid_descriptor());
                b.extend([
                    7,
                    5,
                    0x81,
                    3,
                    if self.kind == Kind::Keyboard { 8 } else { 3 },
                    0,
                    10,
                ]);
                Some(b)
            }
            0x21 => Some(self.hid_descriptor()),
            0x22 => Some(self.report_descriptor().to_vec()),
            _ => None,
        }
    }
    fn request(&self, s: &mut HidState, q: [u8; 8], data: &[u8]) -> Result<Vec<u8>, ()> {
        let value = u16::from_le_bytes([q[2], q[3]]);
        let index = u16::from_le_bytes([q[4], q[5]]);
        let length = u16::from_le_bytes([q[6], q[7]]) as usize;
        let mut reply = match (q[0], q[1]) {
            (0x80, 6) if q[3] == 1 || q[3] == 2 => self.descriptor(q[3], q[2]).ok_or(())?,
            (0x81, 6) if index == 0 && (q[3] == 0x21 || q[3] == 0x22) => {
                self.descriptor(q[3], q[2]).ok_or(())?
            }
            (0, 5) if value < 128 && index == 0 && length == 0 => {
                s.address = value as u8;
                vec![]
            }
            (0, 9) if value <= 1 && index == 0 && length == 0 => {
                s.configured = value == 1;
                s.expected = false;
                s.last = None;
                vec![]
            }
            (0x80, 8) if value == 0 && index == 0 => vec![u8::from(s.configured)],
            (0x80, 0) if value == 0 && index == 0 => vec![0, 0],
            (0x81, 0) if value == 0 && index == 0 => vec![0, 0],
            (0x81, 10) if value == 0 && index == 0 => vec![0],
            (1, 11) if value == 0 && index == 0 && length == 0 => vec![],
            (0xa1, 3) if value == 0 && index == 0 => vec![s.protocol],
            (0x21, 11) if value <= 1 && index == 0 && length == 0 => {
                s.protocol = value as u8;
                vec![]
            }
            (0xa1, 2) if value == 0 && index == 0 => vec![s.idle],
            (0x21, 10) if q[2] == 0 && index == 0 && length == 0 => {
                let remaining = s.due.saturating_sub(s.now);
                s.idle = q[3];
                // HID 1.11 preserves the current interval within four ms of expiry.
                if remaining > 4 {
                    s.due = s.now + u64::from(s.idle) * 4;
                }
                vec![]
            }
            (0xa1, 1) if q[2] == 0 && index == 0 && q[3] == 1 => s.report.clone(),
            (0xa1, 1) if value == 0x200 && index == 0 && self.kind == Kind::Keyboard => {
                vec![s.leds]
            }
            (0x21, 9)
                if value == 0x200
                    && index == 0
                    && length == 1
                    && data.len() == 1
                    && self.kind == Kind::Keyboard =>
            {
                s.leds = data[0] & 31;
                vec![]
            }
            _ => return Err(()),
        };
        reply.truncate(length);
        Ok(reply)
    }
    /// Whole control request, also useful to front ends that operate in HID usages.
    pub fn control(&self, q: [u8; 8], data: &[u8]) -> Result<Vec<u8>, ()> {
        self.request(&mut self.state.lock(), q, data)
    }
    fn interrupt(&self, s: &mut HidState, p: &UsbPacket) -> UsbOutcome {
        if !s.configured {
            return UsbOutcome::Stall;
        }
        if p.toggle != s.expected {
            return s
                .last
                .clone()
                .map(UsbOutcome::Ack)
                .unwrap_or(UsbOutcome::Nak);
        }
        let report = if self.kind == Kind::Keyboard {
            if let Some(report) = s.changes.pop_front() {
                Some(report)
            } else if s.last.is_none() || (s.idle != 0 && p.time_ms >= s.due) {
                Some(s.report.clone())
            } else {
                None
            }
        } else {
            let changed =
                s.last.as_ref().is_none_or(|r| r[0] != s.buttons) || s.dx != 0 || s.dy != 0;
            if changed || (s.idle != 0 && p.time_ms >= s.due) {
                let dx = s.dx.clamp(-127, 127);
                let dy = s.dy.clamp(-127, 127);
                s.dx -= dx;
                s.dy -= dy;
                Some(vec![s.buttons, dx as i8 as u8, dy as i8 as u8])
            } else {
                None
            }
        };
        if let Some(bytes) = report {
            s.expected = !s.expected;
            s.last = Some(bytes.clone());
            s.due = p.time_ms + u64::from(s.idle) * 4;
            UsbOutcome::Ack(bytes)
        } else {
            UsbOutcome::Nak
        }
    }
}
impl UsbFunction for HidFunction {
    fn reset(&self) {
        *self.state.lock() = HidState::new(self.kind);
    }
    fn address(&self) -> u8 {
        self.state.lock().address
    }
    fn transact(&self, p: UsbPacket) -> UsbOutcome {
        let mut s = self.state.lock();
        s.now = p.time_ms;
        if p.endpoint == 1 && p.pid == Pid::In {
            return self.interrupt(&mut s, &p);
        }
        if p.endpoint != 0 {
            return UsbOutcome::Stall;
        }
        if p.pid == Pid::Setup {
            let Ok(q) = <[u8; 8]>::try_from(p.data.as_slice()) else {
                return UsbOutcome::Stall;
            };
            let mut response = vec![];
            let mut stalled = false;
            if q[0] & 128 != 0 {
                match self.request(&mut s, q, &[]) {
                    Ok(r) => response = r,
                    Err(()) => stalled = true,
                }
            }
            s.control = Some(Control {
                setup: q,
                response,
                offset: 0,
                outgoing: vec![],
                expected: true,
                previous: None,
                stalled,
            });
            return UsbOutcome::Ack(vec![]);
        }
        let Some(mut c) = s.control.take() else {
            return UsbOutcome::Stall;
        };
        if c.stalled {
            s.control = Some(c);
            return UsbOutcome::Stall;
        }
        if let Some((pid, toggle, bytes)) = &c.previous {
            if *pid == p.pid && *toggle == p.toggle {
                let result = UsbOutcome::Ack(bytes.clone());
                s.control = Some(c);
                return result;
            }
        }
        if p.toggle != c.expected {
            let result = c
                .previous
                .as_ref()
                .filter(|(pid, t, _)| *pid == p.pid && *t == p.toggle)
                .map(|(_, _, b)| UsbOutcome::Ack(b.clone()))
                .unwrap_or(UsbOutcome::Stall);
            s.control = Some(c);
            return result;
        }
        let length = u16::from_le_bytes([c.setup[6], c.setup[7]]) as usize;
        let reply = if c.setup[0] & 128 != 0 {
            if p.pid == Pid::In {
                let end = (c.offset + p.capacity).min(c.response.len());
                let b = c.response[c.offset..end].to_vec();
                c.offset = end;
                b
            } else if p.pid == Pid::Out && p.data.is_empty() {
                vec![]
            } else {
                c.stalled = true;
                vec![]
            }
        } else if p.pid == Pid::Out && c.outgoing.len() < length {
            if p.data.len() > length - c.outgoing.len() {
                c.stalled = true;
            } else {
                c.outgoing.extend(&p.data);
            }
            vec![]
        } else if p.pid == Pid::In && c.outgoing.len() == length {
            if self.request(&mut s, c.setup, &c.outgoing).is_err() {
                c.stalled = true;
            }
            vec![]
        } else {
            c.stalled = true;
            vec![]
        };
        let result = if c.stalled {
            UsbOutcome::Stall
        } else {
            c.previous = Some((p.pid, p.toggle, reply.clone()));
            c.expected = if c.setup[0] & 128 != 0
                && p.pid == Pid::In
                && (reply.len() < p.capacity || c.offset >= length)
            {
                true
            } else if c.setup[0] & 128 == 0 && p.pid == Pid::Out && c.outgoing.len() == length {
                true
            } else {
                !c.expected
            };
            UsbOutcome::Ack(reply)
        };
        s.control = Some(c);
        result
    }
}
