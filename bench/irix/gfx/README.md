# Graphics acceptance

A repeatable check that a graphics machine is usable the way a person uses
it: boot IRIX with the graphics console, log in at the desktop, compare the
desktop with a golden, play SGI Quake's and SGI Doom's timedemos and run a
Performer demo. The timedemo rates are a real-world performance figure as
well, comparable between heads of iris on the same host and disks.

```sh
IRIS_BIN=target/release GFX_PROM=... GFX_DISK=... GFX_GAMES=... \
    bench/irix/gfx/run.sh fuel results/fuel
```

`run.sh` writes a config for the machine, starts iris, waits for the
console login, logs in as root and runs `steps.toml` with `iris-bench irix
--strict`. `MACHINE.toml` adds what differs per machine: the desktop golden,
and on the slower machines a longer Quake timeout. The steps files hold the
guest commands; the script only starts and stops the emulator. Each step ends as one of:

- **ok**, with its host time and, for the timedemos, the game's own figure;
- **FAILED**, when the command exits nonzero, hangs past its timeout, or its
  screenshot does not match;
- **SKIP**, when the program it needs is not on the disk.

A missing system disk or PROM prints `SKIP gfx/MACHINE` and exits 0. A
missing games disk skips the Quake and Doom steps. Anything failed makes
the script exit 1. The result JSON, every screenshot, the generated config,
the console log and iris's stderr land in the output directory, and
`iris-bench report` renders the result.

## What runs

| Step | Checks | Figure |
|---|---|---|
| `gfx/x_server` | Xsgi starts after the console login | seconds to X |
| `gfx/login` | `key type root`, `key enter` at the xdm greeter (through the monitor), then the toolchest starts | seconds to the desktop |
| `gfx/desktop` | the screen is within `tolerance` of the step's `golden` | distance |
| `gfx/quake` | SGI Quake's `+timedemo demo1` finishes; a screenshot 45 s in differs from the desktop | Quake's fps, in guest seconds |
| `gfx/doom` | SGI Doom's `-timedemo demo1` finishes; a screenshot 25 s in differs from the desktop | game tics per host second over the demo; the tic count (5026 when the demo plays as recorded) |
| `gfx/perfly` | `perfly truck.pfb` still runs after 60 s and its window differs from the desktop | |

A golden is a signature, not an image: the mean colour of each cell of a
16x12 grid, with the frame size. `iris-bench screen-sig FILE [OTHER]`
prints one, and the distance between two screenshots, in mean levels per
colour channel. To record a golden, run once without one (the step then
prints the signature it saw), look at the screenshot, and paste the
signature into the step. To see a wrong golden fail, give the run an extra
steps file that replaces the desktop step:

```toml
[[step]]
name = "gfx/desktop"
unit = "op"
cmd = "sleep 10"
screenshot = "desktop"
golden = "sig1 16x12 1280x1024 000000..."   # any other signature
```

```sh
bench/irix/gfx/run.sh fuel results/fuel-broken --extra-steps wrong-golden.toml
```

## Preparing the disks

Nothing here ships with iris. IRIX, Performer and the SGI game ports are
commercial or third-party software and the game data is id Software's, so
the test runs only against disks the user provides.

**System disk** (`GFX_DISK`), raw. An IRIX 6.5 install for the machine
with the X server enabled (`chkconfig windowsystem on`, the default), a
getty on the serial console so `iris-ci login` can reach a root shell
while the greeter is on screen, a root account with no password, and
OpenGL Performer's execution environment and demos (`performer_eoe`,
`performer_demo`, `performer_data`, from the IRIX 6.5 Applications or
Foundation CDs). The disks these steps were checked on:

| Machine | Disk | Performer |
|---|---|---|
| Fuel | IRIX 6.5.30, IP35 | 3.2.2 |
| Octane | IRIX 6.5.30, IP30, IMPACT | 3.2.2 |
| Indy | IRIX 6.5.22, IP24 | as installed with 6.5.22 |

**Games disk** (`GFX_GAMES`), raw, optional, fitted as SCSI ID 2 and
mounted at `/g`. It is an IRIX XFS disk carrying, under `/usr/games`:

- `sgiquake/quake.gl`: SGI's OpenGL port of Quake, from
  `ftp.sgi.com/sgi/quake/download/quake1/sgiquake.tar.gz`. Licence: id
  Software's Quake source terms in its `LICINFO.TXT`. It carries no game
  data.
- `tyrquake/id1/pak0.pak`: Quake's shareware data, which SGI Quake plays
  from (the steps start it in `tyrquake`). The shareware `pak0.pak` is in
  id Software's `quake106.zip`; any copy of it in `tyrquake/id1` works.
- `sgidoom/sgixdoom` and `sgidoom/doom1.wad`: SGI Doom 1.8, id Software's
  1995 IRIX binary release (`doom-1.8.tar.gz`), with the shareware WAD.

The disk the steps were checked with is an IRIX 6.5.30 Octane2 install
with these files added; any IRIX disk with that layout under `/usr/games`
does. Its root partition must be
partition 0 (`/dev/dsk/dks0d2s0`).

The game steps use no sound: Quake runs with `-nosound`, and Doom finds a
stand-in sound server that only waits, since not every machine has audio.

## Machines

| Machine | Config | Default board |
|---|---|---|
| `fuel` | `fuel_ip34`, R14000, 512 MB, L1 console | VPro V12 |
| `octane` | `octane_ip30`, R10000, 256 MB | IMPACT (SolidImpact), the iris default |
| `indy` | `indy_ip24`, R4400, 256 MB | Newport |

`GFX_BOARD` fits another board the machine takes. The Fuel needs an NVRAM
(`GFX_NVRAM`) and boot flash (`GFX_FLASH`) whose environment boots the disk;
the Octane and Indy PROMs find the disk from an empty NVRAM.

## Writing steps

Each wrapped command line must stay under 200 characters, about 85 of
command; a test checks it. iris-ci types its end marker straight after the
command, the Fuel's L1 console hands both lines to IRIX at once, and IRIX's
tty drops typeahead past 255 characters. A command that loses the end of
its marker runs into its timeout. Long command lines go into scripts in
`/tmp`, built by short `echo` steps.

## Status

Run on 2026-11-08 with the disks above and the games disk, three guests at
once on one host, so the rates move with host load (Doom on the Octane gave
116.5 tics/s in an earlier run and 93.7 here):

| Machine | iris | Desktop | Quake | Doom | Performer | Result |
|---|---|---|---|---|---|---|
| Fuel, V12 | fuel-ip35 6de594c34 | 0.0 from golden | 8.2 fps, 132 s | 60.3 tics/s, 5026 tics | drawn | pass |
| Octane, IMPACT | bench-irix-pr 931c1c826 | 0.0 from golden | 0.3 fps, 65 min | 93.7 tics/s, 5026 tics | drawn | pass |
| Indy, Newport | bench-irix-pr 931c1c826 | 0.0 from golden | 0.4 fps, 43 min | 82.9 tics/s, 5026 tics | drawn, untextured | pass |

Quake's GL path on the IMPACT and Newport is slow enough that their
timedemo takes most of an hour, so those machine files allow it 90
minutes. The Fuel's desktop came out 0.0 from its golden in three runs.
With a wrong golden the desktop step failed and the run exited 1; without
the games disk Quake and Doom were skipped.
