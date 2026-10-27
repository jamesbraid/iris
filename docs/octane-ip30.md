# SGI Octane (IP30)

The Octane is SGI's 1997 workstation: one or two R10000s on HEART (the memory
controller and interrupt hub), a Crossbow switch (XBOW) joining the widgets,
the onboard Bridge (XIO to PCI) carrying two QLogic ISP1040 SCSI controllers
and the IOC3 I/O chip (serial ports, keyboard and mouse, the clock, Ethernet),
and a graphics board on one of the XIO ports. IRIS emulates a single-processor
Octane with a 195 MHz R10000.

Status: TODO: checkpoint (what boots, to where, with which PROM and IRIX
release; filled in from recorded boot evidence).

The Octane does not run in iris-gui yet; start it from the `iris` command line.

## Configure the machine

An Octane is set up with the shared keys. `[octane]` exists for what only an
Octane has and is empty for now.

```toml
prom  = "IP30prom.bin"           # the stock IP30 PROM (not built in)
nvram = "nvram.bin"              # the DS1685 clock's NVRAM: setenv settings live here
banks = [256, 256]               # MB in each HEART bank
serial_port_a = 2291             # serial port 1, the console
serial_log = "serial.log"

[machine]
profile = "octane_ip30"

[[port_forward]]
proto = "tcp"
host_port = 2292
guest_port = 22

[scsi.1]
path = "irix6530.raw"
```

Paths are relative to the directory iris runs in. Every shared key is listed in
[FEATURES.md](../FEATURES.md).

`cpu` must be `r10000` (the default for this profile is the only choice).
`[clock] fixed_mhz` sets CP0 Count; unset it is 97.5 MHz, half of the 195 MHz
R10000's pipeline clock.

## PROM

The PROM image is not built in and is not shipped with iris. Supply the stock
IP30 PROM as `prom` (left unset it is `prom.bin`); a file that cannot be read
stops iris before the CPU runs, naming the path. The image is at most 1 MiB
(the flash size) and is never written. IRIS has been tested with an unmodified
image only. TODO: checkpoint (PROM revision tested).

## Memory

`banks` lists the megabytes in each of HEART's four banks (each a matched pair
of DIMMs). Valid sizes are 64, 128, 256 and 512; fill the banks from the first
with no gap, 2 GB at most. The default `[128, 128]` is 256 MB.
TODO: checkpoint (sizes tried).

## Disks

The `[scsi.N]` tables ([FEATURES.md](../FEATURES.md#scsiid)) list the devices
on the Octane's two ISP1040 controllers. `N` 1-7 is the id on the internal
bus (controller 0); `N` = 16 + id is that id on the external bus (controller
1), or use `controller = 1` in a low-numbered table. IRIX expects its system
disk at id 1 on controller 0. Raw images, `cdrom = true`, `overlay = true`
and the iris-ci scratch volume work as on the O2 ([STORAGE.md](../STORAGE.md)).
An image that cannot be opened stops iris before the PROM runs.

The controllers are modelled as the host sees them: the mailbox protocol, the
request and response queues, and the RISC RAM download. The controller's own
firmware is stored and never run; SCSI commands go straight to iris's shared
target model, with automatic sense. TODO: checkpoint (disk boot, CD-ROM).

## NVRAM

The PROM keeps `setenv` settings in the NVRAM of the IOC3's DS1685 clock.
`nvram` names the file it is kept in between runs (`nvram.bin` unless set):
iris starts from it when it has the right size and writes it back whenever the
guest changes it. An Octane and another machine must not share the file.

## Console, keyboard and mouse

The console is serial port 1 on TCP 127.0.0.1 (`serial_port_a`, 8880 unless
set; `telnet 127.0.0.1 PORT`), and `serial_log` copies what it sends to a
file. Port 2 is `serial_port_b`. Set the PROM's `console` variable to `d` for
a serial console. The IOC3's PS/2 keyboard and mouse are modelled; there is no
window until graphics is attached (below), so use the serial console.

## Networking

The IOC3's Ethernet (an ICS1890 PHY at MII address 1) is on iris's NAT
gateway, as on the other machines, with the same settings: `nat_subnet`,
`[nfs]`, `[[port_forward]]`, `[network] mac` and `tftp_dir`. The guest gets
192.168.0.2 by DHCP, with the gateway and DNS at 192.168.0.1. When
`[network] mac` is unset the address is the placeholder 08:00:69:12:34:56; set
your own if more than one Octane shares a network. `[network] mode = "off"`
leaves the Ethernet unplugged; `pcap` is not available. TODO: checkpoint
(network boot or ping, DHCP, ssh).

## Graphics

The graphics board is Solid IMPACT on widget 11, the Octane's SI board:
the same raster chips as the Indigo2 IP28's Solid IMPACT, behind the Octane's
XIO host interface (the HQ4). TODO: checkpoint (whether graphics is attached
in this build and what it shows).

## Scripting and inspection

The monitor (`monitor_port`) has, besides the usual CPU and device commands:

- `heart [status|unsupported]`: HEART's state, and the registers a guest
  touched that are not modelled.
- `ip30 unmapped`: processor accesses that hit nothing (first 64, with the
  program counter). `ip30 xbow`: unsupported Crossbow accesses.

`IRIS_IP30_TRACE=1` in the environment prints the first unmapped accesses and
HEART's bank mapping as they happen. `machine-stop`, `machine-start`, `reset`
and `gdb_port` work as on the other machines. `iris --ci` starts the Octane
for [iris-ci](../STORAGE.md) with its CPU stopped and serial port 1 as the CI
console. TODO: checkpoint (iris-ci verified or not).

## What does not work yet

- Single processor only.
- RAD1 audio (PCI slot 3), the PCI expansion slots, and the Octane's other
  XIO widgets are not modelled.
- No snapshots (`save`/`load`).
- HEART's ECC error injection is not modelled; a mode write that asks for it
  is recorded as unsupported.
- HEART's low-memory behaviour is a functional approximation of the Origami
  model: 16 KiB of distinct RAM at physical 0, the first 16 KiB of SDRAM space
  a bus error, and the PROM's bank-size probe answered by the model rather than
  by real DIMM serial data. A cached CPU access through the direct RAM window
  does not see that bus error.
- Crossbow link training is not modelled: a fitted link comes up alive at
  once.
- The Bridge's processor access to its long translated window, and 64-bit PCI
  DMA addressing, are not modelled.
- ISP1040: the RISC firmware never runs; some mailbox and host commands are
  accepted without effect and noted in the log.
- IOC3 Ethernet: a non-zero receive timer reload is not modelled, only an
  immediate timeout.
- Board identity records (serial numbers, barcodes) are synthetic.
- Not on the Octane yet: iris-gui, `rollback`.
