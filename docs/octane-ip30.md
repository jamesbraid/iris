# SGI Octane (IP30)

The Octane is SGI's 1997 workstation: one or two R10000s on HEART (the memory
controller and interrupt hub), a Crossbow switch joining the widgets, the
onboard Bridge (XIO to PCI) carrying two QLogic ISP1040 SCSI controllers, the
IOC3 I/O chip (serial ports, keyboard and mouse, the clock, Ethernet) and RAD
audio, and a graphics board on one of the XIO ports. IRIS emulates a
single-processor Octane with a 195 MHz R10000.

With the stock IP30 PROM (the 879,312-byte image, SHA-256 `ee4722ca6f9b821f…`)
and an installed IRIX 6.5.30 disk, the machine boots to a serial login.
`hinv` reports the processor, HEART revision D, 128 MB, Crossbow 1.3, both
QL1040B controllers with the disk at unit 1 on controller 0, both IOC3 serial
ports, the parallel port, `ef0` and the RAD audio processor. With the Solid
IMPACT board fitted (the default) and a blank NVRAM, the PROM comes up on
the graphics console and IRIX starts X to the login screen and a desktop.
The guest pings
the NAT gateway, fetches files from its TFTP server and accepts telnet logins
through a port forward. `reboot` comes back to the login prompt. The
"/ns: Resource busy" line during startup comes from the guest; the disk
prints it on the other Octane emulator too.

The Octane does not run in iris-gui; start it from the `iris` command line.

## Configure the machine

An Octane is set up with the shared keys; `[octane]` holds what only an
Octane has.

```toml
prom  = "IP30prom.bin"        # the stock IP30 PROM (not built in)
nvram = "nvram-ip30.bin"      # the DS1685 clock's NVRAM: the PROM's settings
banks = [128]                 # MB in each HEART bank
headless = true               # no window; console=d in the NVRAM for a serial console
serial_log = "serial.log"
nat_subnet = "192.0.2.0/24"

[machine]
profile = "octane_ip30"
cpu = "r10000"

[octane]
flash = "flash-ip30.bin"      # keep the PROM's flash records between runs
# graphics = "none"           # no graphics board (default: "solidimpact")

[[port_forward]]
proto = "tcp"
host_port = 2323
guest_port = 23

[scsi.1]
path = "irix6530.raw"
overlay = true
```

Paths are relative to the directory iris runs in. Every key is listed in
[FEATURES.md](../FEATURES.md). `machine.cpu` must be `r10000`. `[clock]
fixed_mhz` sets CP0 Count; unset it is 97.5 MHz, half of the R10000's
195 MHz pipeline clock.

## PROM and flash

The PROM image is not built in and is not shipped with iris: `prom` must name
the stock IP30 PROM, at most 1 MiB. It is loaded into the boot flash, a 1 MiB
part with the Intel/Sharp command set on the Bridge's first chip select,
where the PROM also keeps its persistent records. `[octane] flash` names a
file holding the whole flash, so those records survive from run to run; it
is created from `prom` when missing or empty, and an existing file of
exactly 1 MiB is used as it stands (delete it after changing `prom`). A
file of any other size is refused with an error and left untouched. Without it the flash starts from
`prom` every run.

## Memory

`banks` lists the megabytes in each of HEART's four banks (each a matched
pair of DIMMs). Valid sizes are 64, 128, 256 and 512; fill the banks from the
first with no gap, 2 GB at most. The shared default `[128, 128]` is 256 MB.
The PROM sizes the banks itself; IRIX has been booted with 128 MB.

## NVRAM

The PROM keeps its environment (`console`, `AutoLoad`, boot paths) in the
NVRAM of IOC3's DS1685 clock, including its extended RAM. `nvram` names the
file it is kept in (512 bytes); left at the shared default `nvram.bin` the
Octane uses `nvram-ip30.bin`. A blank NVRAM makes the PROM pick the graphics
console; set `console=d` (or start from an image that has it) for the serial
console.

## Disks

The `[scsi.N]` tables list the devices on the two ISP1040s: `N` 1-7 is the
id on the internal bus (controller 0), `N` = 16 + id that id on the external
bus (controller 1), or use `controller = 1`. IRIX expects its system disk at
id 1 on controller 0. Raw images and `overlay = true` work as on the other
machines ([STORAGE.md](../STORAGE.md)). The controller's own firmware is
stored and never run; SCSI commands go to iris's shared target model. The
monitor's `scsi` and `cow` commands reach controller 0 (`scsi1`, `cow1` the
external bus).

## Console, keyboard and mouse

The console is IOC3's first serial port on TCP 127.0.0.1 (`serial_port_a`,
8880 unless set), and `serial_log` copies what it sends to a file; the second
port is `serial_port_b`. The PROM's `console` variable picks between the
serial and graphics consoles; `headless` only removes the window.

## Graphics

The graphics board is Solid IMPACT on widget 11: the same raster chips as
the Indigo2 IP28's Solid IMPACT behind the Octane's XIO host interface (the
HQ4), with interrupts and DMA through HEART. Its screen is the iris window
and what iris-ci screenshots show; the keyboard and mouse are IOC3's PS/2
ports. `[octane] graphics = "none"` leaves the port empty, and the machine
then runs on its serial console with no window.

## Networking

The IOC3 Ethernet (an ICS1890 PHY at MII address 1) is on iris's NAT
gateway, with the usual settings: `nat_subnet`, `[nfs]`, `[[port_forward]]`,
`[network] mac` and `tftp_dir`. The gateway answers on host .1 of
`nat_subnet`, or, until the first packet routes, on whatever address in its
/24 the guest asks for: an IRIX disk configured as 192.0.2.2 with a default
route of 192.0.2.3 works with `nat_subnet = "192.0.2.0/24"` unchanged. When
`[network] mac` is unset the station address is 08:00:69:12:34:56; set your
own if more than one Octane shares a network. `[network] mode = "off"`
unplugs the Ethernet; `pcap` is not available.

## Inspecting the machine

The monitor (`monitor_port`) has, besides the usual CPU and device commands:

- `heart status`: HEART's interrupt bits, mask, counter and bank descriptors;
  `heart unsupported`: HEART registers a guest touched that are not modelled.
- `ip30 unmapped`: physical accesses that failed (first 64, with the PC);
  `ip30 xbow`: refused Crossbow register accesses.
- `scsi notes`: what the ISP1040 driver did that the model noted.

`IRIS_IP30_TRACE=1` prints failed accesses, unsupported HEART registers and
HEART's bank mapping as they happen.

## What is approximated or not modelled

- One processor only.
- HEART: the answer to the PROM's bank-size probe is an approximation; ECC
  error injection is not modelled (a mode write that asks for it is
  recorded); the first 16 KiB of SDRAM space is a bus error and 16 KiB of
  distinct RAM sits at physical 0, which a cached access through the CPU's
  direct RAM window does not see.
- The PROM's cached stores to its own data in the flash are kept in a
  per-line copy, as a write-back cache would hold them; they are never
  written back.
- Crossbow link training is not modelled: a fitted link that is reset comes
  back alive at once.
- The Bridge's processor access to its long translated window and peer DMA
  between widgets other than HEART are not modelled.
- ISP1040: the firmware never runs; some mailbox commands are accepted
  without effect and noted.
- RAD audio has no host audio output.
- Board identity records carry made-up barcodes and serial numbers.
- No snapshots, and not in iris-gui.

## Sources

The device models are ports of James Braid's
[Origami](https://github.com/jamesbraid/origami) project's models from its
QEMU `sgi-origami` branch (<https://github.com/jamesbraid/qemu-sgi-origami>),
BSD 3-Clause, with the same behaviour and approximations; each file names the
QEMU source it came from. The boot flash derives from MAME's `intelfsh.cpp`
(BSD 3-Clause, Aaron Giles). The Bridge, Xbridge and Crossbow specifications
are in [docs/Octane/](Octane/).
