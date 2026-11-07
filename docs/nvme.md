# NVM Express controller

iris can fit one NVM Express controller in the O2 or the Octane. It is written
from the NVM Express Base Specification (revision 1.4) and the PCI
specifications; it models the specification and nothing vendor-specific. A
guest needs a driver for it: IRIX has none of its own, and Linux and the BSDs
carry theirs.

```toml
[nvme]
path = "nvme.img"     # raw or CHD; the card is present when this is set
overlay = true        # writes go to nvme.img.overlay, as for [scsi.N]
bridge = false        # true: behind a PCI-to-PCI bridge in the slot
```

## Where it sits

| Machine | Slot | Interrupt |
|---|---|---|
| O2 | "PCI option", device 3 | INTA, CRIME input 10 |
| Octane | Bridge slot 5 | INTA, Bridge pin 5 |

With `bridge = true` the slot holds a PCI-to-PCI bridge (type 1 header) and
the controller is device 0 on the bus behind it. Firmware numbers the bus and
opens the bridge's memory window; a pin of the card reaches the bridge's pins
by the usual swizzle ([docs/pci.md](pci.md)). The card's DMA is the slot's.

## What it does

- Controller registers in a 64-bit memory BAR0 (8 KiB): CAP, VS 1.4, INTMS,
  INTMC, CC, CSTS, AQA, ASQ, ACQ, and doorbells with a 4-byte stride.
- Admin commands: Create and Delete I/O Submission and Completion Queue,
  Identify (controller, namespace, active namespace list, namespace
  descriptors), Get Log Page (error information, SMART / health, firmware
  slot), Get and Set Features, Abort, Async Event Request.
- NVM commands: Read, Write, Write Zeroes, Flush on one namespace of 512-byte
  blocks, PRP data pointers, up to 128 KiB a command.
- Up to 16 I/O queue pairs of 2 to 1024 entries, physically contiguous
  (CAP.CQR). Queue 0 first, then the others in turn.
- Pin-based interrupts only (the PCI core has no MSI): the pin is asserted
  while a completion queue with interrupts enabled holds an entry the host has
  not read and INTMS bit 0 is clear. Vector 0 is the only vector.

Anything else is refused with the status the specification gives it. The
monitor's `nvme refused` lists the opcodes and features a guest asked for that
were refused; `nvme status` shows the registers, the queues and the counters;
`nvme cow ...` is the copy-on-write layer's commands for the namespace.

## Approximations

- Enable, disable and shutdown complete at once; CAP.TO (5 s) is only the
  bound a host may wait.
- Interrupt coalescing is stored and reported but not applied.
- The PCI Express and Power Management capabilities are present and
  read-only.
- The namespace is at most 2 TiB: the disk layer addresses blocks in 32 bits.
- The namespace GUID is made from the image path.
- A doorbell value outside its queue is ignored and counted; no asynchronous
  error event is raised.
- The PCI vendor and device numbers (1b36:0010) are the ones QEMU's emulated
  controller uses.

## Storage

The namespace is a disk of the shared SCSI target layer, so images, CHD,
copy-on-write overlays, the background sync and the `cow` commands are those
of [STORAGE.md](../STORAGE.md). A snapshot saves the controller's state and
the overlay with the other disks, and records the disk's size under slot 240
in its manifest.
