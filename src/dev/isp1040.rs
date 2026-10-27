//! The QLogic ISP1040 SCSI host adapters on the Octane's Bridge: a
//! `PciBusDevice` on `crate::scsi`, its ring thread started in
//! `Device::start` and joined in `Device::stop`.
