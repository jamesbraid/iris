# discreet-integration: a branch for running Discreet Flame on the Octane

Status: working note. This branch is never sent upstream. It is rebuilt from
its parts whenever upstream main moves, so do not commit to it directly:
change a part and rebuild.

## What it holds

1. `upstream/main`.
2. The three commits of `discreet-scsi-identity` (a `[scsi.N]` table sets a
   drive's INQUIRY vendor, product, revision and serial; iris-gui shows
   them; the bytes after a serial are NUL-filled). Discreet's Stone+Wire
   software compares INQUIRY bytes 36 to 43 with a configuration and checks
   byte 44 onwards against the serial, so a space-padded serial makes it
   re-install its disk configuration in the middle of a write.
3. One test fix: the ISP12160 disc-status test still called the old
   seven-argument `attach_device`.
4. `mgras: keep the host DMA page tables across a context switch`: the
   HQ3 pool page tables are one set for the board, so the X server's
   desktop repaint after Flame exits copies through Flame's freed table
   and comes out black, as the guest gets on hardware.

The Octane clock fix (techomancer/iris#501) is already in upstream main
(`374416e2`), so it is not carried.

## Rebuild

```sh
git fetch up
git checkout -B discreet-integration up/main
git cherry-pick 98ccc122 a288676d a2fc2fde      # discreet-scsi-identity
git cherry-pick <the isp12160 test fix>
git cherry-pick <the mgras page-table change>
```

Cherry-picks keep each part reviewable alone and drop out by themselves once
upstream has them.

## Running Flame 7.0 on it

These are settings, not code:

- `[network] mac = "08:00:69:0e:0e:1a"`: the Octane's host ID is derived
  from the Ethernet address, and Discreet licences are tied to it. The
  default MAC gives a different host ID and Flame reports `NOT LICENSED FOR
  PLATFORM`.
- The licence dongle is a serial device. iris serves Octane port B on
  `serial_socket_b` (raw bytes, one client). `tools/discreet-dongle` in
  the Origami repository also listens on a Unix socket, so bridge the two
  with `socat UNIX-CONNECT:<dongle.sock> UNIX-CONNECT:<serial_socket_b>`,
  started after iris is up.
- Stone disks: `vendor = "STON+WIR"` and an 8-character `serial` on each.
- Stone+Wire's `sw_host_map` names the guest's IP address. The NAT guest is
  192.0.2.2, so a map written under another address (QEMU's is 192.0.2.15)
  fails with `Could not find System Host ID`.
- A guest clock before 2004-10-03 (Stone+Wire's own limit).
