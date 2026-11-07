//! A SCSI controller as a socket map handle: what the target rows under a
//! controller row are placed on.
//!
//! Controllers own their bus by value, so the row holds the controller (as the
//! small [`DiskController`] view) and reaches the bus through it, with no extra
//! load on the command path. A controller that can host a DaynaPort also
//! carries the closure that builds one, since that needs the machine's network.

use std::io;
use std::sync::Arc;

use super::ScsiBus;
use crate::config::DaynaportParams;
use crate::disks::DiskController;
use crate::workstation::DiscChanger;

type DaynaportFn = Box<dyn Fn(u8, DaynaportParams) -> io::Result<()> + Send + Sync>;

pub struct ScsiPort {
    ctl: Arc<dyn DiskController>,
    dayna: Option<DaynaportFn>,
}

impl ScsiPort {
    pub fn new(ctl: Arc<dyn DiskController>) -> ScsiPort {
        ScsiPort { ctl, dayna: None }
    }

    /// Say how this controller builds a DaynaPort at a target id.
    pub fn with_daynaport(mut self, build: impl Fn(u8, DaynaportParams) -> io::Result<()> + Send + Sync + 'static) -> ScsiPort {
        self.dayna = Some(Box::new(build));
        self
    }

    pub fn bus(&self) -> &ScsiBus {
        self.ctl.bus()
    }

    pub fn controller(&self) -> &Arc<dyn DiskController> {
        &self.ctl
    }

    /// The controller's CD-ROM drives, for disc changes.
    pub fn changer(&self) -> Arc<dyn DiscChanger> {
        self.ctl.clone().changer()
    }

    pub fn add_daynaport(&self, id: u8, params: DaynaportParams) -> io::Result<()> {
        match &self.dayna {
            Some(build) => build(id, params),
            None => Err(io::Error::new(io::ErrorKind::Unsupported, "this controller has no network")),
        }
    }
}
