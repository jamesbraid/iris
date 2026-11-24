//! The part of iris's `display` the board names: a head to be damaged and attached. The refresh thread that
//! would drive it is iris's; here a head does nothing.

use std::sync::{Arc, Weak};

use crate::status::MachineStatus;

/// What a board offers the refresh thread. Nothing here calls it, so any board is one.
pub trait Head: Send + Sync {}
impl<T: Send + Sync> Head for T {}

pub struct Refresh;

impl Refresh {
    pub fn head(&self, _status: Arc<MachineStatus>) -> Arc<HeadHandle> {
        Arc::new(HeadHandle)
    }
}

pub struct HeadHandle;

impl HeadHandle {
    /// The picture may differ: nothing to wake.
    pub fn damage(&self) {}

    pub fn attach(&self, _head: Weak<dyn Head>) {}
}
