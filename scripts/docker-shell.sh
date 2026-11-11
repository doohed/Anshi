#!/usr/bin/env bash
# Builds docker/Dockerfile and drops into an interactive shell inside it
# with this repo bind-mounted at /work -- the way to build and run Anshi
# on a host that isn't x86_64 Linux (an Apple Silicon Mac, say).
#
# The container is an *environment*, not a build system: once inside, the
# normal entry points are still the normal entry points --
# ./scripts/build-iso.sh then ./scripts/run-qemu.sh, exactly as documented
# for a native Linux host. The image just supplies a clang/ld.lld
# cross-toolchain (via CC/LD in its ENV) and an x86_64 QEMU.
#
# Any arguments are run inside the container instead of an interactive
# shell, e.g.:
#   ./scripts/docker-shell.sh ./scripts/build-iso.sh
#
# Requires: docker.
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE="anshi-dev"

if ! command -v docker >/dev/null 2>&1; then
    echo "error: docker not found -- install Docker Desktop (macOS) or docker.io" >&2
    exit 1
fi

docker build -t "$IMAGE" -f docker/Dockerfile .

# -it so the serial console works both ways: run-qemu.sh's -serial stdio
# is how you actually talk to the Anshi shell, so the container needs a
# real TTY, not just captured output.
exec docker run --rm -it -v "$(pwd):/work" -w /work "$IMAGE" "$@"
