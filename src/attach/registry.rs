//! Every device type, in one list. Adding a type costs its `DeviceType` impl
//! beside the device and one line here.

use super::Maker;
use crate::dev::nvme::NvmeCard;
use crate::dev::testdev::TestDeviceType;
use crate::dev::ultra64::Ultra64Card;
use crate::dev::virtio::mmio::VirtioSerialGio;
use crate::dev::virtio::pci::VirtioSerialPci;
use crate::dev::xio_pci::{PciCage, PciShoehorn};
use crate::pci::PciBridgeCard;
use crate::scsi::media::{Cdrom, Daynaport, Hdd};

pub static MAKERS: &[Maker] = &[
    Maker::of::<Hdd>(),
    Maker::of::<Cdrom>(),
    Maker::of::<Daynaport>(),
    Maker::of::<NvmeCard>(),
    Maker::of::<PciBridgeCard>(),
    Maker::of::<VirtioSerialPci>(),
    Maker::of::<Ultra64Card>(),
    Maker::of::<TestDeviceType>(),
    Maker::of::<VirtioSerialGio>(),
    Maker::of::<PciCage>(),
    Maker::of::<PciShoehorn>(),
];
