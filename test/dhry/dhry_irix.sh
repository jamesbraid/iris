#!/bin/sh
# Build Dhrystone 2.1 on IRIX 6.5 with MIPSpro cc.
# Copy this file, dhry.h, dhry_1.c and dhry_2.c onto the IRIX guest, then run
# it there. -DTIME selects the whole-second time(2) clock, which is how the
# prebuilt binary was made, so give the program at least ~20 s of runs.
set -e
cc -O2 -n32 -mips3 -s -DTIME dhry_1.c dhry_2.c -o dhrystone
echo "built: ./dhrystone"
