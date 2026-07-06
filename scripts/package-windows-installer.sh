#!/usr/bin/env bash
set -euo pipefail

TARGET="x86_64-pc-windows-gnu"
APP_NAME="dicron"
STAGING_DIR="dist/windows/app"
INSTALLER_SCRIPT="packaging/windows/dicron.nsi"

copy_dll_to_staging() {
    local dll_name="$1"
    local dll_path=""

    dll_path="$(x86_64-w64-mingw32-g++ -print-file-name="$dll_name" || true)"

    if [[ "$dll_path" == "$dll_name" || ! -f "$dll_path" ]]; then
        echo "ERROR: Could not find $dll_name from active MinGW runtime."
        echo "Try:"
        echo "  x86_64-w64-mingw32-g++ -print-file-name=$dll_name"
        exit 1
    fi

    echo "Copying $dll_name from $dll_path"
    cp "$dll_path" "$STAGING_DIR/"
}

if [[ ! -f Cargo.toml ]]; then
    echo "ERROR: Run this from the project root where Cargo.toml exists."
    exit 1
fi

if ! command -v x86_64-w64-mingw32-objdump >/dev/null 2>&1; then
    echo "ERROR: x86_64-w64-mingw32-objdump not found."
    echo "Install:"
    echo "  sudo apt install -y binutils-mingw-w64-x86-64"
    exit 1
fi

if ! command -v makensis >/dev/null 2>&1; then
    echo "ERROR: makensis not found."
    echo "Install:"
    echo "  sudo apt install -y nsis"
    exit 1
fi

EXE_PATH="$(find "target/$TARGET/release" -maxdepth 1 -type f -name '*.exe' | head -n 1)"

if [[ -z "$EXE_PATH" ]]; then
    echo "ERROR: Windows exe not found. Run scripts/build-windows.sh first."
    exit 1
fi

rm -rf "$STAGING_DIR"
mkdir -p "$STAGING_DIR"

cp "$EXE_PATH" "$STAGING_DIR/$APP_NAME.exe"

DLL_OUTPUT="$(x86_64-w64-mingw32-objdump -p "$EXE_PATH" | grep "DLL Name" || true)"

echo
echo "Direct DLL dependencies from exe:"
echo "$DLL_OUTPUT"
echo

EXTRA_DLLS="$(echo "$DLL_OUTPUT" | sed -n 's/.*DLL Name: \(lib.*\.dll\)/\1/p' | sort -u)"

for dll_name in $EXTRA_DLLS; do
    copy_dll_to_staging "$dll_name"
done

# Indirect MinGW runtime DLLs used by libstdc++ / CharLS builds.
for runtime_dll in \
    libstdc++-6.dll \
    libgcc_s_seh-1.dll \
    libwinpthread-1.dll
do
    copy_dll_to_staging "$runtime_dll"
done

echo
echo "Staged files:"
ls -lh "$STAGING_DIR"

echo
echo "Building installer..."
APP_VERSION="$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json, sys; print(json.load(sys.stdin)["packages"][0]["version"])')"
makensis "-DAPP_VERSION=$APP_VERSION" "$INSTALLER_SCRIPT"

echo
echo "DONE:"
echo "  dist/DicronSetup-$APP_VERSION.exe"