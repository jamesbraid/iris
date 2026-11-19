//! The interrupt-controller trait `interface.rs`'s HEART packet sender
//! names. The engine's own sender puts packets in its outbox instead.

pub trait InterruptController: Send + Sync {
    fn set_line(&self, line: u32, level: bool);
}
