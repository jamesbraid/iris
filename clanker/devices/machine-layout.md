# How the machines share Devices, Disks, Control and GuestNet (design)

Status: in place for the Indy-family (`Ip22Machine`), the O2 and the Octane.

## The pieces

- `Control` (`src/control.rs`): lifecycle state (never `Running`; `status`
  reads that from the CPU), the machine-event channel, the nullable
  `MachineSlot` the monitor, event and CI threads reach the machine through.
  One `unsafe` contract: the slot holds a raw pointer from `as_ws_ptr`, and
  `Control::release` is the first statement of every machine's `Drop` and
  takes the slot's lock, so a command in flight finishes before anything is
  torn down. A second `register` panics. A wrapper type implementing
  `Workstation` must forward `as_ws_ptr` to the machine it wraps.
- `GuestNet` (`src/guest_net.rs`): the NAT controls, the guest address
  triple, the frame counter. The Indy-family builds one with `from_parts`
  from its SEEQ (always present); the O2 and Octane use `open` (none when
  `[network] mode = "off"`).
- `Disks` (`src/disks.rs`): flush, pending CHD syncs, the CD-ROM changer, over
  the SCSI controllers a machine passes in bus order.
- `Devices` (`src/devices.rs`): the parts a machine starts, stops, resets,
  saves and loads, in registration order. `Workstation` gives each machine
  provided methods over five accessors (`control`, `control_mut`, `net`,
  `monitor`, `as_ws_ptr`) plus `devices` and `disks`.

## Rules that are easy to break

- **A part that owns others cascades.** The HPC3 starts, stops and powers on
  the IOC, RTC, EEPROM, SEEQ, SCSI and HAL2; the IOC the SCC, PIT and PS/2;
  `Ip32Bus` its SCSI controllers. The machine registers only the parent, so
  nothing is started or reset twice. `Saveable::children` names the children
  for snapshots only, so the files keep the names they have always had.
- **Start, stop and load order.** Start and load run in registration order,
  children before their parent; stop runs the I/O parts then the chipset
  backwards and the display last (a display stop joins a thread that can fail
  to end, and the disks must be stopped by then).
- **Interrupt levels across a load.** Two controllers re-derive their lines
  after a load, by different means, and both are on purpose.
  - The IOC loads after the SCC, PIT and PS/2 it owns, so its saved status is
    what it holds; `Ioc::load_state` then calls `Z85c30::redrive`, because the
    SCC stands in for the character in flight at save with a pending transmit
    interrupt that the saved status may lack.
  - HEART loads before the devices that publish a level into it (the Octane
    used to load it last). A leaf's load writes its level into HEART as a
    level, which is idempotent, and HEART's own load rebuilds pending, masks
    and the CPU pins from its saved registers, so the order does not matter
    for a consistent snapshot (`octane_load_order_does_not_matter`). An
    inconsistent one, such as all-ones in every part, is order-dependent.
  A new interrupt controller picks one of these and says so in its load.
- **Trait defaults.** `Device`, `Saveable` and `Resettable` have no-op
  defaults, so a part writes `impl Saveable for X {}` for what it does not
  have. An empty saved table means no state: `Devices` leaves the key out.
- **`optional`.** Said once, where a part is registered (`add_optional`) or on
  a `Child`: the snapshot may lack the key. Everything else is required, and
  `load` checks the key set before it touches a part.
- **Reset.** `power_on_devices` is the CPUs and every part; the Octane's
  `power_on_all` adds RST# to the PCI side (`Bridge::reset_pci`).

## Tests and the 64-table limit

A process has 64 device-map tables, and a dropped machine must give its table
back: an Indy-family machine's drop stops VINO's thread, releases the bus's
devices and the HPC3's DMA wiring (the lock registry holds its owners weakly
for the same reason). An O2 does not yet: its CRIME-RE thread holds the bus
for the life of the process, so each O2 a test builds keeps its table. Tests
that only read a machine share one through `workstation_tests::with_shared`
(one O2 for the whole binary), and tests that change or drop a machine use an
Octane or an Indy-family machine. Never raise `MAX_MAPS`.
