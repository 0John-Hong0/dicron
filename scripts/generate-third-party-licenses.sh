#!/usr/bin/env bash
set -euo pipefail

# Generates the license report for the Rust crates compiled into Dicron with
# cargo-about. The release workflow ships it in every package.

OUTPUT_PATH="${1:-dist/THIRD_PARTY_LICENSES.html}"

if [[ ! -f Cargo.toml ]]; then
    echo "ERROR: Run this from the project root where Cargo.toml exists."
    exit 1
fi

if ! command -v cargo-about >/dev/null 2>&1; then
    echo "ERROR: cargo-about not found. Install it with:"
    echo "  cargo install --locked cargo-about --features cli"
    exit 1
fi

mkdir -p "$(dirname "$OUTPUT_PATH")"

cargo about generate --locked --fail -o "$OUTPUT_PATH" about.hbs

echo
echo "DONE:"
echo "  $OUTPUT_PATH"
