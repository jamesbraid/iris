#!/bin/sh
# Build the C Whetstone on IRIX 6.5 with MIPSpro cc.
# Copy this file and whetstone.c onto the IRIX guest, then run it there.
set -e
cc -O2 -n32 -mips3 -s whetstone.c -lm -o whetstone
echo "built: ./whetstone"
