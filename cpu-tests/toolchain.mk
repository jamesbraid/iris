# toolchain.mk — the MIPS cross toolchain both bare-metal suites build with.
#
# One compiler builds every guest binary in the repository: cpu-tests, bench,
# and the bench image iris embeds. The benchmark measures rates of compiled
# guest code, so the compiler is part of the workload: GCC 14.2 runs Dhrystone
# in 428 guest instructions per iteration where GCC 16.2.1 needs 313. The pin
# below is the only place the version is written; toolchain.Containerfile and
# CI take it from here.
#
#   make -C cpu-tests toolchain-image    build the pinned toolchain container
#   make -C <suite> container-<target>   run `make <target>` inside it
#   make CROSS=<prefix>                  use a different compiler anyway
#
# Fedora 45 is the only distribution packaging GCC 16.2 as a MIPS cross
# compiler; Debian and Ubuntu are dropping theirs. Its release repository
# never changes, so these exact versions stay installable until Fedora 45's
# end of life on 2027-12-08. After that the repository moves to
# https://archives.fedoraproject.org/pub/archive/fedora/linux/releases/45/ —
# bump to a newer release or point the container there before then. Bumping
# changes every benchmark figure: rebuild bench/prebuilt in the same commit and
# say that results from either side are not comparable.

GUEST_TC_BASE     := docker.io/library/fedora:45@sha256:a7d72ba2745b100a3351087472e5ee7b2450af7cf331e7673007e3817612839c
GUEST_TC_PACKAGES := gcc-mips64-linux-gnu-16.2.1-1.fc45 \
                     binutils-mips64-linux-gnu-2.47-1.fc45 \
                     cross-gcc-common-16.2.1-1.fc45
GUEST_GCC_VERSION := 16.2.1
GUEST_CROSS       := mips64-linux-gnu-

# The repository root, for the container's mount and for -ffile-prefix-map:
# the suites record paths relative to it, so a build's bytes do not depend on
# where the checkout lives.
REPO_ROOT := $(abspath $(dir $(lastword $(MAKEFILE_LIST)))..)

# Search order when CROSS is not given: the pinned triple first, then the
# names other distributions use (any version; see the warning below).
# A candidate is accepted only if `$(name)gcc --version` actually runs — a
# broken/dangling shim on PATH (crossdev leaves these behind sometimes) must
# not shadow a working toolchain further down the list.
cc_works = $(shell $(1)gcc --version >/dev/null 2>&1 && echo yes)

ifeq ($(origin CROSS), undefined)
  ifeq ($(call cc_works,$(GUEST_CROSS)),yes)
    CROSS := $(GUEST_CROSS)
  else ifeq ($(call cc_works,mips-linux-gnu-),yes)
    CROSS := mips-linux-gnu-
  else ifeq ($(call cc_works,mips-unknown-linux-gnu-),yes)
    CROSS := mips-unknown-linux-gnu-
  else ifeq ($(call cc_works,mips64-unknown-linux-gnu-),yes)
    CROSS := mips64-unknown-linux-gnu-
  else
    CROSS := $(GUEST_CROSS)
  endif
endif

CC      := $(CROSS)gcc
LD      := $(CROSS)ld
OBJCOPY := $(CROSS)objcopy
OBJDUMP := $(CROSS)objdump
READELF := $(CROSS)readelf
NM      := $(CROSS)nm

# A different compiler still builds, for quick local runs, but its binaries
# are not the ones CI tests and its benchmark figures are not comparable.
CC_VERSION := $(shell $(CC) -dumpfullversion 2>/dev/null)
ifneq ($(CC_VERSION),)
  ifneq ($(CC_VERSION),$(GUEST_GCC_VERSION))
    $(warning $(CC) is GCC $(CC_VERSION), not the pinned $(GUEST_GCC_VERSION); use container-<target> for the pinned build)
  endif
endif

.PHONY: toolchain-check
toolchain-check:
	@command -v $(CC) >/dev/null 2>&1 || { \
	  echo "error: $(CC) not found."; \
	  echo "  With Docker or Podman: make -C $(REPO_ROOT)/cpu-tests toolchain-image,"; \
	  echo "  then make container-<target> instead of <target>."; \
	  echo "  Fedora 45 natively: dnf install $(GUEST_TC_PACKAGES)"; \
	  exit 1; }
	@echo "toolchain: $$($(CC) --version | head -1)"

# ── the pinned toolchain in a container ──────────────────────────────────────

CONTAINER    ?= $(shell command -v podman 2>/dev/null || command -v docker 2>/dev/null)
GUEST_TC_TAG := iris-guest-toolchain:$(GUEST_GCC_VERSION)
# Write files as the invoking user: rootless Podman maps it with keep-id,
# Docker needs the ids passed explicitly.
ifneq ($(findstring podman,$(CONTAINER)),)
  CONTAINER_USER := --userns=keep-id
else
  CONTAINER_USER := --user $(shell id -u):$(shell id -g)
endif
# This suite's directory inside the container, where the repository is /src.
SUITE_DIR := $(patsubst $(REPO_ROOT)/%,%,$(CURDIR))

.PHONY: toolchain-image
toolchain-image:
	@[ -n "$(CONTAINER)" ] || { echo "error: neither podman nor docker is installed"; exit 1; }
	$(CONTAINER) build -t $(GUEST_TC_TAG) \
	  --build-arg BASE=$(GUEST_TC_BASE) \
	  --build-arg PACKAGES="$(GUEST_TC_PACKAGES)" \
	  -f $(REPO_ROOT)/cpu-tests/toolchain.Containerfile $(REPO_ROOT)/cpu-tests

# Run `make <target>` for this suite inside the pinned toolchain. On macOS
# with Colima, the checkout must be under $HOME: Colima shares only that with
# its VM, and anywhere else mounts empty.
container-%: toolchain-image
	$(CONTAINER) run --rm $(CONTAINER_USER) -v $(REPO_ROOT):/src -w /src/$(SUITE_DIR) \
	  $(GUEST_TC_TAG) make CROSS=$(GUEST_CROSS) $*
