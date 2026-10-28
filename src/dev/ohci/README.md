# Lucent OHCI and basic HID functions

The controller identifies as PCI `11c1:5802`, revision `10`, class `0c0310`,
with one 4-KiB memory BAR, INTA and two root ports. Register accesses are aligned
dwords. DMA is little-endian byte traffic through the attached `PciDma` object;
the controller does not reach around the PCI bridge to physical memory.

The implementation adapts descriptor and list structure from MAME's
[src/mame/shared/xbox_usb.cpp](https://github.com/mamedev/mame/blob/2ee9b1e30299b122b131cab0bd7912abc2b095ba/src/mame/shared/xbox_usb.cpp)
and `xbox_usb.h`, copyright Samuele Zannoli, BSD-3-Clause. The retained notice
is in `LICENSE-MAME`. No QEMU OHCI controller or Lucent wrapper is an
implementation input.

Published contracts supply the selected behavior independently: OpenHCI 1.0a
(September 1999), printed pages 17, 20–25, 33–37, 90, 102–106, 108 and 111–131;
Lucent USS-312 Advance Data Sheet revision 5 (March 2000), Tables 27–48;
and HID 1.11 (June 27, 2001), pages 48–54, 59–63 and 74.

Subsystem IDs `ffff:ffff` and PCI reset Status `0200` are compatibility choices
for the selected board, not hardware measurements. OpenHCI's OwnershipChange
status bit 30 is selected because Lucent Table 30's bit 31 conflicts with its
own enable tables and OpenHCI. Interrupt status bit 31 is not an alias for OC.
Legacy presence remains reported, but legacy keyboard emulation and SMI
routing are unsupported.

## Scheduling approximation

This is a functional scheduler, without wire-bandwidth or subframe completion
conformance. A frame has `FI + 1` synthetic units. Each ED visit costs exactly
256 units, including a skipped, halted or empty endpoint, and attempts at most
one general TD packet. PID, length, speed and USB outcome do not change that
cost. Zero-length packets consume the same budget. The cost also bounds cyclic
and malformed lists without inventing a DMA error.

At each frame event, service is batched. Nonperiodic visits run while the
remaining budget exceeds `min(PeriodicStart, FI + 1)`. A visit may cross that
threshold by at most 255 units. The selected periodic HCCA chain then runs
while a visit fits. Nonperiodic work consumes any remainder afterwards.
PeriodicStart at or above the frame budget starts periodic service immediately;
zero permits nonperiodic work to consume the budget first.

`CBSR + 1` control packet attempts earn one bulk attempt. NAK, STALL, error and
no-response are attempts. Skipped, halted and empty EDs consume visit budget
but no ratio credit, and yield to the other ready queue on the next visit.
An absent or disabled queue yields to the other. Credits reset each frame and
persist across its periodic phase. After a visit, the current pointer advances
to NextED; queued work retains CLF/BLF so a zero current pointer can reload its
head. No-fit defers work without retiring it. Periodic exhaustion leaves the
unvisited PeriodCurrentED visible, sets SO and increments SOC modulo four.

Frame events follow the programmed interval in 12-MHz units, accumulated by
millisecond ticks. FI/FIT writes load at the next frame boundary. The timer and
register reads catch up elapsed host time. FmRemaining/FRT expose the last
synthetic service-budget remainder (saturated to 14 bits) and active FIT,
not an accurate wire-bit countdown. At FI zero no ED visit fits. Changing FI
therefore changes frame frequency and visit budget, while packet length and
speed deliberately do not model physical bandwidth.

## Transactions, faults and functions

General control, bulk and periodic TDs use packet-sized transfers, two
nonadjacent buffer pages, per-packet toggles, short packets, NAK, STALL,
third-error retirement and acknowledged overrun behavior. Completed TDs link
in reverse order. DI and WDH prevent a new HCCA publication until software
acknowledges the preceding one. Both transitions of frame bit 15 raise FNO.

Refused payload DMA sets UE and stops schedule/frame progress. Outgoing
prefetch completes before calling a function. An IN may already have changed
its function before host writeback fails. The failing TD is not advanced and
no later list is processed. This is a compatibility policy stricter than the
published system-error requirement; partially completed DMA is not rolled back.

Descriptor and HCCA DMA refusals also stop with UE. Writes already accepted
remain visible; USB side effects remain consumed. In particular, WDH is set
only after successful HCCA DoneHead publication. The exact partial descriptor
and HCCA failure state has not received independent evidence clearance and
remains outside acceptance. After classifying the fatal stop, the controller
latches PCI Detected Parity Error. Software clears that bit by writing one;
fixed PCI Status fields survive both the error and its acknowledgement.

Transactions require a unique matching function on a powered, enabled,
non-suspended, non-resetting root port. Multiple eligible functions at address
zero produce NoResponse without invoking either function. This is an explicit
unsupported-topology approximation, not a model of electrical collision physics
or a last-reset-port selector. Assigning distinct addresses restores routing.

Root power follows NPS/PSM/PPCM selection. Port reset lasts ten milliseconds.
Both ports support attach/detach, enable and change acknowledgement. Hardware
reset resets the controller and attached functions; HCR preserves the root
hub and connection state. Suspend/resume timing remains unsupported because
the available OpenHCI resume-duration text has an unresolved unit defect.

The basic HID keyboard uses eight-byte reports and a one-byte LED Output
report. The mouse uses three buttons and signed relative X/Y. Functions start
in report protocol, support explicit boot/report selection, class requests,
idle suppression/expiry and duplicate-toggle retries. Mouse motion is consumed
only by an acknowledged interrupt report. Host input uses identified HID usages
and raw relative motion; no host keycode-to-usage conversion is supplied.
USB function vendor/product zeros are synthetic placeholders, not a claim to
an assigned physical product identity.

Isochronous EDs remain logged and unretired. External hubs, USB storage,
physical passthrough, electrical fault injection, SMI routing and active-device
snapshots are unsupported. Successful unit tests do not establish Fuel firmware
enumeration, guest input, reconnect acceptance or complete OHCI/USB conformance.
