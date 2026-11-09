# Dhrystone 2.1

`dhry.h`, `dhry_1.c` and `dhry_2.c` are Reinhold P. Weicker's version 2.1 of
25 May 1988, unmodified from <https://www.netlib.org/benchmark/dhry-c>. They
carry no licence text and netlib states no redistribution terms, so their status
here is unconfirmed.

`dhrystone.n32` is an IRIX 6.5 N32 MIPS-III build made by `dhrystone_irix.sh`
(MIPSpro cc 7.4.4m). `dhrystone_linux.sh` builds a host `dhrystone` to compare
with. The program times itself with `time(2)` in whole seconds and refuses to
report under about 2 s, so give it at least 10M runs.
