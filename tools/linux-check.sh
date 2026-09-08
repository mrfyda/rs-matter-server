#!/usr/bin/env sh
# Typecheck the Linux-only code paths from a non-Linux dev machine.
#
# A macOS `cargo check` never compiles anything behind
# `cfg(not(target_os = "macos"))` — the built-in mDNS responder, interface
# selection and socket binding in `ws/`, and the BLE transport. This runs the
# same check inside an arm64 Linux container, which is native on Apple Silicon
# rather than emulated.
#
# Named volumes cache the cargo registry and the target directory, so the first
# run pays for rs-matter and later ones finish in seconds. Arguments are passed
# through to cargo, e.g.:
#
#   tools/linux-check.sh --features bluetooth
#   tools/linux-check.sh --all-targets
#
# The container has no Bluetooth adapter and no BlueZ on its D-Bus bus, so this
# proves the code compiles, not that it talks to a radio. That needs real
# hardware.
set -eu

IMAGE=${LINUX_CHECK_IMAGE:-rust:1-slim-bookworm}
REPO=$(cd "$(dirname "$0")/.." && pwd)

# rs-matter's generated cluster code makes a single rustc want more than 2 GiB
# with debug info on, and Docker Desktop's VM is smaller than the host. A
# typecheck needs no debug info, and capping the job count keeps the peak from
# being multiplied by the core count. Raise LINUX_CHECK_JOBS on a bigger VM.
JOBS=${LINUX_CHECK_JOBS:-2}

exec docker run --rm \
  -v "$REPO:/src:ro" \
  -v rs-matter-server-linux-registry:/usr/local/cargo/registry \
  -v rs-matter-server-linux-target:/target \
  -e CARGO_TARGET_DIR=/target \
  -e CARGO_PROFILE_DEV_DEBUG=0 \
  -e CARGO_BUILD_JOBS="$JOBS" \
  -w /src/server \
  "$IMAGE" \
  cargo check --locked "$@"
