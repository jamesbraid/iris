# TFTP server tests

Exercise IRIS's built-in TFTP server (`crates/iris-dev/src/net/tftp.rs`) from a guest.

- `tftpget.c`: minimal RFC 1350 read client, plain C89 + BSD sockets.
  `cc -o tftpget tftpget.c` on IRIX, `gcc -o tftpget tftpget.c` on Linux.
  Octet and netascii, retransmits, strict TID and block checks; prints
  bytes, blocks, retries, time and a checksum. `-e` expects an ERROR.
- `mkfiles.sh`: builds the corpus in `shared/tftpboot` (host side). Under
  the NFS share, so the guest can `cmp` against the same file.
- `run.sh`: guest-side cases (block-boundary sizes, nested and absolute
  names, netascii, refused escapes, and `/usr/bsd/tftp` when present).
  Exit status = number of failures.

```
sh test/tftp/mkfiles.sh                       # host
iris --config ... --tftp-dir shared/tftpboot  # or tftp_dir under [network]
# guest, with /shared mounted:
mkdir -p /var/tmp/tftptest && cp /shared/tftptest/* /var/tmp/tftptest/   # after copying test/tftp/* to shared/tftptest
cd /var/tmp/tftptest && cc -o tftpget tftpget.c && sh run.sh
```

PROM path: at the Command Monitor, `boot -f bootp()/unix` with a kernel at
`shared/tftpboot/unix` (copy the guest's `/unix`) loads it over TFTP and
boots IRIX. Reach the monitor with `ps2 esc` from the monitor console during
"Starting up the system".
