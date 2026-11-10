# Machine identity

IRIX builds `sysinfo -s` and FLEXlm's `lmhostid` from a number the machine's
hardware gives it, and node-locked licenses match it. Each emulated machine
gets its own, the same on every boot and plainly not a real SGI's unless you
chose it.

Ported from docs/system/mips/sgi/identity.rst in jamesbraid/qemu-sgi-origami.

## The system id

Each machine derives its 32-bit system id one way:

- **Indy, Indigo2, O2, Octane, Origin 200:** the last four bytes of the
  station address, which must start `08:00:69`, so the id starts `69`.
- **Origin 2000, Onyx2:** the low nibble of each of the first ten characters
  of the module serial, keeping the low 32 bits. IRIX is expected to read a
  module serial only when it starts with `K`.

## The keys

| Key | Default | Meaning |
| --- | --- | --- |
| `[machine] emu_id` | `0` | 0 to 4095. Mints an id in the layout below. 0 is the generic id. |
| `[machine] serial` | unset | 1 to 10 printable characters, none of `:` or `;`, as written on a unit (`M2100887`). Sets the id by the fold above. On the Origin 2000 and Onyx2 it must start with `K`. |
| `[machine] hostid` | unset | 8 hex digits, the id itself. On the Indy, Indigo2, O2, Octane and Origin 200 it must start `69`, and 6 digits (the low 24 bits, `69` implied) also work. |
| `[network] mac` | from `emu_id` | The station address, as before. On the Indy, Indigo2, O2, Octane and Origin 200 it is also the id. |

Precedence, highest first: `serial`, `hostid` or `mac`; then `emu_id`; then
`emu_id` 0. Two inputs at the same level must agree, or the machine refuses to
start with a message that names both and the rule, and suggests an input that
is accepted. An address outside `08:00:69` is refused on all of those
machines, because IRIX brings ec0 down otherwise (see below). The start-up line, for example
`iris: host id 69e20000 (station address 08:00:69:e2:00:00)`, says what the
guest will print.

## The layout

| Machine | Value | Host id |
| --- | --- | --- |
| Indy, Indigo2, O2, Octane, Origin 200 | `08:00:69:E2:`*emu_id >> 4*`:`*(emu_id & 15) << 4* | `69E2`, *emu_id*, 0 |
| Origin 2000, Onyx2 | `KEMU2`, the `emu_id` as four digits, the module number | `d52`, *emu_id*, module |

The `2` is iris's kind digit. `emu_id` 42 on an Octane reads
`08:00:69:e2:02:a0`, host id `69e202a0`; on an Origin 2000 `KEMU200420`, host
id `d5200420`. A node or port adds its number to the address, up to 16 without
carrying into the `emu_id`.

On the Origin 2000 and Onyx2 a `hostid` becomes the serial `KK` and eight
characters (`a` to `f` spelled `J` to `O`), and a short `serial` is padded on
the left with `K`. Both are model choices, not hardware facts.

## Where a machine keeps it

- **Indy:** the NVRAM `eaddr` slot. **Indigo2, IP28:** the board EEPROM. Both
  are written only when blank, so a saved one keeps its address.
- **O2:** the identity chip, a DS2502: the address reversed, then `0xff` (an
  approximation), and the ROM serial `45 4d 55 02 00 00`.
- **Octane, Origin 200, Origin 2000, Onyx2:** the IOC3's one-wire
  Ethernet-address part; the Origin 2000 and Onyx2 also carry the module
  serial on the module's identity part.

## What iris guests printed

IRIX 6.5 on iris, with a fresh NVRAM each time (`sysinfo -s` prints decimal):

| Machine | Inputs | `sysinfo -s` | `nvram eaddr` |
| --- | --- | --- | --- |
| O2, R5000 | `hostid = "69100887"` | 1762658439 (`69100887`) | `08:00:69:10:08:87` |
| O2, R10000 | `emu_id = 42` | 1776419488 (`69e202a0`) | `08:00:69:e2:02:a0` |
| Octane | `hostid = "69aabbcc"` | 1772796876 (`69aabbcc`) | `08:00:69:aa:bb:cc` |
| Indy | `hostid = "690abcde"`, `mac = "08:00:69:0a:bc:de"` | 1762311390 (`690abcde`) | `08:00:69:0a:bc:de` |
| Indy | nothing set | 1776418816 (`69e20000`) | `08:00:69:e2:00:00` |

`sysinfo -s` is always the last four bytes of the station address, even when
IRIX rejects the address. Boots of the Indy with only `mac` set, before iris
refused such addresses:

| `mac` | ec0 |
| --- | --- |
| `08:00:69:12:34:56`, `08:00:69:e2:00:00`, `08:00:69:ff:ff:ff` | up |
| `00:80:19:12:34:56`, `0a:00:69:12:34:56`, `02:12:34:56:78:9a`, `08:00:68:12:34:56`, `08:01:69:12:34:56` | down: `ec0: machine has bad ethernet address`, right after the banner |

Any one-byte change in bytes 0 to 2 fails and bytes 3 to 5 do not matter; the
PROM says nothing. An Indigo2 behaved the same for `08:00:69:12:34:56` (up) and
`02:12:34:56:78:9a` (down). The IP28 was not booted, so its refusal is by family
and unverified.

The Indigo2, IP28, Origin 200, Origin 2000 and Onyx2 were not booted with a
chosen identity; unit tests pin what their devices hold. Their rules are
expectations, notably that the Origin 2000 and Onyx2 need a `K` module serial.
