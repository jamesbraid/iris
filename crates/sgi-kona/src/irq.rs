//! The part of iris's `irq` the mounted files name: where an interrupt
//! packet goes. The engine's own implementation queues it in the outbox.

pub trait InterruptController: Send + Sync {
    fn set_line(&self, line: u32, level: bool);

    /// A crosstalk interrupt packet that widget `source` sent to `address`,
    /// naming vector `line`; whether the host took it.
    fn packet(&self, _source: u8, _address: u64, line: u32, level: bool) -> bool {
        self.set_line(line, level);
        true
    }

    /// A packet for widget `widget`, which is not the host: whether something routes it.
    /// None does by default, and the packet is counted and dropped.
    fn foreign_packet(&self, _source: u8, _widget: u8, _address: u64, _line: u32, _level: bool) -> bool {
        false
    }
}
