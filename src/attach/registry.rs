//! Every device type, in one list. Adding a type costs its `DeviceType` impl
//! beside the device and one line here.

use super::Maker;
use crate::dev::testdev::TestDeviceType;
use crate::dev::ultra64::Ultra64Card;
use crate::dev::virtio::mmio::VirtioSerialGio;
use crate::scsi::media::{Cdrom, Daynaport, Hdd};

pub static MAKERS: &[Maker] = &[
    Maker::of::<Hdd>(),
    Maker::of::<Cdrom>(),
    Maker::of::<Daynaport>(),
    Maker::of::<Ultra64Card>(),
    Maker::of::<TestDeviceType>(),
    Maker::of::<VirtioSerialGio>(),
];
