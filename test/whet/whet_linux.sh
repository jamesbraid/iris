#!/bin/sh
# Build the C Whetstone on Linux for a host baseline. Writes whetstone.host,
# not whetstone, to leave the checked-in IRIX binary alone.
set -e
cd "$(dirname "$0")"
cc -std=gnu89 -O2 -w whetstone.c -lm -o whetstone.host
echo "built: $(pwd)/whetstone.host"
