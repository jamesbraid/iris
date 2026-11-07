# Devices and where they go

A disk, a CD-ROM, a DaynaPort and the add-in cards are devices. Each goes at a
**location**: a dotted path that names one place on the machine, the same way
in the config file, on the command line, in error messages, in snapshots and in
the GUI.

```toml
[[hdd]]
at = "hpc3.0.t1"          # Indy and Indigo2: first controller, target 1
path = "irix.img"

[[cdrom]]
at = "hpc3.0.t4"
path = "6.5.22-1.iso"
discs = ["6.5.22-2.iso"]  # more discs for the drive's changer
```

`iris --list-slots [--machine PROFILE] [--config FILE]` prints every place the
machine has, what is in it and what it takes; the monitor's `slots` command
prints the same for the running machine.

## Locations

A location is a path of hops from the machine, written with dots. A hop is a
word and a number, and numbers are local to the thing that owns them, so a
location depends only on the machine and the config, never on the order
devices attach in.

| Hop | Meaning |
|---|---|
| `hpc3` | The Indy and Indigo2 I/O chip. `hpc3.0` and `hpc3.1` are its SCSI controllers. |
| `gio0`, `gio1` | The GIO expansion slots, in address order (`0x1F400000`, `0x1F600000`). `exp0` and `exp1` are accepted as the old spelling. |
| `gfx` | The graphics slot. |
| `xio15` | A widget on the Octane's XIO fabric, by hardware number. |
| `pci0` | PCI bus 0 of the thing it hangs off. |
| a bare number | A position on that bus: a PCI device number, or an `hpc3` controller. |
| `t4` | SCSI target id 4. Id 0 is the controller and is not a place. |

| Machine | Places |
|---|---|
| Indy | `hpc3.0.t1` to `t7`, `gio0`, `gio1`, `gfx` |
| Indigo2, Indigo2 IMPACT | the same, plus `hpc3.1.t1` to `t7` for the second controller |
| O2 | `pci0.1.t1` to `t7` (internal controller), `pci0.2.t1` to `t7` (external connector) |
| Octane | `xio15.pci0.0.t1` to `t15` (internal controller), `xio15.pci0.1.t1` to `t15` (external), `xio15.pci0.4` for virtio-serial |

A place that is built in (a controller, the IOC3) refuses a device and says what
is there. The second Newport head, when asked for with `graphics.heads = 2`,
fills `gio1`.

## Device types

| Type | Goes on | Options |
|---|---|---|
| `hdd` | a SCSI target | `path` (required), `overlay`, `scratch`, `size_mb`, `write_cache` |
| `cdrom` | a SCSI target | `path` (may be empty: an empty tray), `discs` |
| `daynaport` | a SCSI target of an Indy or Indigo2 | `mac`, `subnet` |
| `virtio_serial` | `gio0`, `gio1`; on the Octane `xio15.pci0.4` | `[[virtio_serial.port]]` tables |
| `ultra64` | `gio0` | none |
| `test_device` | `gio0` | `dump` |

A wrong option is refused with the options the device type has:
`error: pci0.1.t1: unknown field `speed`, expected one of `path`, `overlay`, `scratch`, `size_mb`, `write_cache` for hdd`.
Two entries at one location, or an entry on a place that takes another kind of
device, are refused too, naming the place.

## On the command line

One flag places any device type, as the config does: `--device
hdd@pci0.1.t1,path=irix.img`, `--device cdrom@pci0.1.t4,path=a.iso,discs=b.iso`.
It makes the same entry a `[[TYPE]]` table with `at` makes, and the device type
parses it the same way. The first `=` in an item splits it, so a path may
contain more; a list option repeats; a nested option is a TOML value
(`--device 'virtio_serial@gio1,port=[{ name = "a", tcp_port = 9000 }]'`). The flags that predate this
keep their meaning and name a target on the machine's first controller:
`--scsi1/2/3/7 FILE`, `--cdrom4/5/6 FILE`, `--cdrom4/5/6-extra ISO`,
`--test-device`. A flag replaces the file's entry at the same location.

A file that places no disk, CD-ROM or DaynaPort gets `scsi1.raw` at target 1
and `cdrom4.iso` at target 4 of the first controller. An empty `[scsi]` table
says "none".

## Old config files

The `[scsi.N]` tables, `[ultra64]`, `test_device`, `test_device_dump` and
`[virtio_serial]` still load. Once the machine is known, iris rewrites them
into the entries above and prints one notice for each:
`notice: [scsi.17] is now [[hdd]] at = "pci0.2.t1"`. The GUI saves
the new form.

| Old | New |
|---|---|
| `[scsi.N]`, N 1 to 15 | target N of the controller `controller` names (default 0) |
| `[scsi.16+N]` | target N of controller 1 |
| `cdrom = true` or `kind = "cdrom"` | `[[cdrom]]` |
| `kind = "daynaport"` | `[[daynaport]]` |
| `[ultra64] enabled = true` | `[[ultra64]]` at `gio0` |
| `test_device = true`, `test_device_dump = "f"` | `[[test_device]]` at `gio0`, `dump = "f"` |
| `[virtio_serial] enabled = true`, `slot = "exp1"` | `[[virtio_serial]]` at `gio1` (`exp0` is `gio0`) |

The controllers of each machine are:

| Machine | Controller 0 | Controller 1 |
|---|---|---|
| Indy | `hpc3.0` | none |
| Indigo2, IP28 | `hpc3.0` | `hpc3.1` |
| O2 | `pci0.1` | `pci0.2` |
| Octane | `xio15.pci0.0` | `xio15.pci0.1` |

A file that mixes `[scsi.N]` with `[[hdd]]`, `[[cdrom]]` or `[[daynaport]]` is an
error: use one form.

## Snapshots

Snapshots name disks and devices by location (`hpc3.0.t1.overlay`, a `gio0`
state file), so they are schema 6. A snapshot from an older iris is refused,
naming both versions; boot it with the iris that made it.
