# The pinned MIPS guest toolchain. Every value comes from toolchain.mk, which
# passes them as build arguments; build it with `make -C cpu-tests
# toolchain-image` rather than by hand, so the pin is written in one place.
#
# Fedora installs only from its release repository (--repo=fedora), which
# never changes after release, so the same packages land every time. The base
# is a multi-architecture index digest and runs natively: arm64 and amd64
# builds of these packages produce byte-identical guest binaries, while
# running amd64 under emulation on an arm64 host made cc1 crash at random.
ARG BASE
FROM ${BASE}
ARG PACKAGES
RUN dnf -y --repo=fedora --setopt=install_weak_deps=False install ${PACKAGES} make \
    && dnf clean all
# The suites' Makefiles key on this: it means "the pinned compiler is here".
ENV IRIS_GUEST_TOOLCHAIN=1
