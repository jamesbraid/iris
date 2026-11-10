//! Every device type, in one list. Adding a type costs its `DeviceType` impl
//! beside the device and one line here.

use super::Maker;
use crate::scsi::media::{Cdrom, Daynaport, Hdd};

pub static MAKERS: &[Maker] = &[
    Maker::of::<Hdd>(),
    Maker::of::<Cdrom>(),
    Maker::of::<Daynaport>(),
];
