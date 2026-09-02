#!/usr/bin/env bash
set -euo pipefail

FONT_DIR="assets/fonts"
FONT_FILE="${FONT_DIR}/SourceHanSans-Medium.otf"

# Adobe's official standalone "Source Han Sans Medium", from the 2.005R tag.
# This is the single face src/main.rs embeds. The full collection is 45 faces
# and ~117 MB; every face but this one is unused, and Adobe publishes this one
# on its own, so there is nothing to extract.
FONT_URL="https://raw.githubusercontent.com/adobe-fonts/source-han-sans/2.005R/OTF/Japanese/SourceHanSans-Medium.otf"
FONT_SHA256="377372ded6fd6958c971cc69e8ac5ae2f97140a014f14f3dfc2f281cfb3d2a1b"

mkdir -p "$FONT_DIR"

if [[ -f "$FONT_FILE" ]] && echo "${FONT_SHA256}  ${FONT_FILE}" | sha256sum -c --status -; then
    echo "Already present and verified: $FONT_FILE"
    ls -lh "$FONT_FILE"
    exit 0
fi

if [[ -f "$FONT_FILE" ]]; then
    echo "Replacing $FONT_FILE: checksum does not match the pinned font."
fi

for dep in curl sha256sum; do
    command -v "$dep" >/dev/null || {
        echo "Missing dependency: $dep" >&2
        exit 1
    }
done

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

echo "Downloading Source Han Sans Medium (16.5 MB)..."
curl -fSL "$FONT_URL" -o "${WORK_DIR}/font.otf"

echo "Checking SHA256..."
echo "${FONT_SHA256}  ${WORK_DIR}/font.otf" | sha256sum -c -

mv "${WORK_DIR}/font.otf" "$FONT_FILE"

echo "Ready:"
ls -lh "$FONT_FILE"
