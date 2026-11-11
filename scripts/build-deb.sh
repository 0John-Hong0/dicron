#!/usr/bin/env bash
set -euo pipefail

# Build Debian packages on Ubuntu 20.04 to keep their glibc requirement at
# 2.31 or older. Change BUILD_IMAGE and GLIBC_FLOOR together.
#
# Run from the repository root after scripts/fetch-fonts.sh and
# scripts/generate-third-party-licenses.sh. Requires Docker and writes the
# package to target/debian/.
BUILD_IMAGE="ubuntu:20.04"
GLIBC_FLOOR="2.31"

if [[ "${1:-}" != "--in-container" ]]; then
  exec docker run --rm \
    --volume "${PWD}:/src" \
    --workdir /src \
    --env HOST_UID="$(id -u)" \
    --env HOST_GID="$(id -g)" \
    "${BUILD_IMAGE}" \
    bash scripts/build-deb.sh --in-container
fi

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
  build-essential \
  ca-certificates \
  cmake \
  curl \
  nasm \
  pkg-config

curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal --default-toolchain stable
export PATH="${HOME}/.cargo/bin:${PATH}"

# Build in a copy, so this build does not mix with the host's target directory.
mkdir /build
tar --exclude=./target --exclude=./.git -cf - . | tar -xf - -C /build
cd /build

cargo install cargo-deb --locked
cargo deb

needed="$(objdump -T target/release/dicron | grep -o 'GLIBC_[0-9.]*' | sed 's/^GLIBC_//' | sort -uV | tail -n 1)"
echo "dicron needs glibc ${needed}; the floor is ${GLIBC_FLOOR}."
if [[ "$(printf '%s\n' "${needed}" "${GLIBC_FLOOR}" | sort -V | tail -n 1)" != "${GLIBC_FLOOR}" ]]; then
  echo "dicron needs a newer glibc than ${GLIBC_FLOOR}; build it in an older image." >&2
  exit 1
fi

for dir in /src/target /src/target/debian; do
  if [[ ! -d "${dir}" ]]; then
    mkdir "${dir}"
    chown "${HOST_UID}:${HOST_GID}" "${dir}"
  fi
done
install -o "${HOST_UID}" -g "${HOST_GID}" -m 644 target/debian/*.deb /src/target/debian/
