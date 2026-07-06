#!/usr/bin/env bash
set -euo pipefail

TARGET="x86_64-pc-windows-gnu"

if [[ ! -f Cargo.toml ]]; then
    echo "ERROR: Run this from the project root where Cargo.toml exists."
    exit 1
fi

missing_commands=()

for command_name in \
    rustup \
    cargo \
    x86_64-w64-mingw32-gcc \
    x86_64-w64-mingw32-g++ \
    x86_64-w64-mingw32-ar \
    x86_64-w64-mingw32-windres
do
    if ! command -v "$command_name" >/dev/null 2>&1; then
        missing_commands+=("$command_name")
    fi
done

if [[ ${#missing_commands[@]} -gt 0 ]]; then
    echo "ERROR: Missing commands:"
    printf '  - %s\n' "${missing_commands[@]}"
    echo
    echo "Install:"
    echo "  sudo apt update"
    echo "  sudo apt install -y mingw-w64 gcc-mingw-w64-x86-64 g++-mingw-w64-x86-64 binutils-mingw-w64-x86-64 cmake pkg-config"
    exit 1
fi

rustup target add "$TARGET"

unset RUSTFLAGS || true
unset CARGO_ENCODED_RUSTFLAGS || true
unset CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS || true
unset CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER || true
unset CXXSTDLIB || true
unset CXXSTDLIB_x86_64_pc_windows_gnu || true

CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc \
CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-g++ \
cargo build --release --target "$TARGET"