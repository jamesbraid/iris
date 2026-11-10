# A machine's host id follows its station address; saved NVRAM keeps the old one

**Symptom.** After upgrading, `sysinfo -s` on an existing Indy or Indigo2 still
reports the id of the old default address `08:00:69:12:34:56`, while the
start-up line `iris: host id ...` shows the new one. Or two emulated machines
share a node-locked license host id.

**Cause.** IRIX builds the host id from the last four bytes of the station
address on the Indy, Indigo2, O2, Octane and Origin 200, and from the first ten
characters of the module serial on the Origin 2000 and Onyx2. The default
address changed to `08:00:69:e2:00:00` (`[machine] emu_id` 0). iris writes the
Indy's NVRAM `eaddr` slot and the Indigo2's EEPROM words only when they are
blank, so a saved `nvram.bin` or `nveeprom.bin` keeps the address it holds. The
O2 stores no copy and takes the new address on its next boot.

**Rule.**

- To change a saved Indy's or Indigo2's address, set it in the PROM
  (`setenv -f eaddr ...`) or delete the saved NVRAM or EEPROM file. The
  start-up line says "an address already saved in NVRAM or EEPROM is kept".
- To keep the old default, set `[network] mac = "08:00:69:12:34:56"`.
- `hostid`, `serial` and `mac` name one id on the machines that take it from
  the address, so they must agree. See [machine identity](../../docs/machine-identity.md)
  for the keys and layout.
- The Indy, Indigo2 and IP28 refuse a `mac` outside `08:00:69` at start-up. The
  evidence is Indy and Indigo2 boots: IRIX logs
  `ec0: machine has bad ethernet address` right after the banner and brings
  ec0 down for any change in the first three bytes. See the table in
  [machine identity](../../docs/machine-identity.md). IP28 was not booted;
  its refusal is by family and unverified.
- The Origin 2000 and Onyx2 module serial rule (IRIX reads it only when it
  starts with `K`) is an expectation, not yet confirmed on an iris boot.
