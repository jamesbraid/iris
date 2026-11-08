#!/bin/sh
# Build Dhrystone 2.1 on Linux for a host baseline. The sources are K&R C, so
# ask for gnu89. Writes dhrystone.host, not dhrystone, to leave the checked-in
# IRIX binary alone.
set -e
cd "$(dirname "$0")"
cc -std=gnu89 -O2 -w -DTIME dhry_1.c dhry_2.c -o dhrystone.host
echo "built: $(pwd)/dhrystone.host"
