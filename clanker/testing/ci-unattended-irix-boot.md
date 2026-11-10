# Unattended IRIX boot under --ci: seed NVRAM, composite screenshots on the CPU

**Keywords:** ci,boot-test,nvram,setenv,screenshot,black,headless,console,SystemPartition
**Category:** testing

Two things stop a scripted `--ci` boot of an installed disk from working out of
the box. `scripts/ci-boot-test.sh` handles both; read it before writing another.
The `boot-test` job in `.github/workflows/build.yml` runs it.

## A fresh NVRAM can't boot anything

With no `nvram.bin` (or a bad checksum) the PROM prints `NVRAM checksum is
incorrect: reinitializing.` and resets to defaults that have **no
`SystemPartition`/`OSLoadPartition`**. AutoLoad still fires, and on a graphics
console it stops at `Cannot load /sash. No default device and path in
environment.` iris injects only the MAC (`backdoor_set_mac_if_blank`), never
the boot path.

Fix: a headless pass first. With `--headless` the PROM falls back to the serial
console, which the CI socket can drive: wait `Option?` → `5` → `setenv -f
SystemPartition scsi(0)disk(1)rdisk(0)partition(8)`, `OSLoadPartition
...partition(0)`, `OSLoadFilename /unix`, `AutoLoad Yes` → `iris-ci rtc-save`
→ quit. The next run auto-boots the disk unattended. The script also sets
`console d`, so `login:` arrives on serial; the framebuffer stays dark (irix-install.md §8).

The script then seeds NVRAM again with `console g` and boots a second time.
The login is drawn on the framebuffer and serial stays silent, so `iris-ci
screen-wait` polls screenshots until one matches
`scripts/boot-goldens/indy-irix53.png`. It counts pixels with a channel more
than 32 away from the golden and allows 1000. On the IRIX 5.3 login screen
the mouse and text cursors account for up to 100 between frames; a missing
window or a half-drawn screen is hundreds of thousands. The golden must be a
screenshot of the disk CI boots: a different install shows other users and
another hostname. To replace it, take `gfx-screen.png` from a failing run's
`boot-test-*` artifact once you have checked it is the login screen.

## Screenshots under --ci

`cmd_screenshot` (src/ci.rs) takes its frame from `gfx_display::fresh_frame`.
With no front-end, Newport composes in software on its refresh thread, so this
needs no GPU and works on headless CI runners.

The first screenshot right after `start` fails with `the display has no video
mode yet`: VC2 timings aren't programmed. `screen-wait` treats that as not
matching yet and keeps polling.

## Other gotchas hit on the way

- macOS caps a Unix socket path at 104 bytes (`path must be shorter than
  SUN_LEN`); keep `ci_socket` under `/tmp`, not a deep workspace path.
- A config with no `[scsi.1]` falls back to `scsi1.raw` and iris exits fatally
  if it's missing.
