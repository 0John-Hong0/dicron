#!/usr/bin/env bash
set -euo pipefail

FONT_DIR="assets/fonts"
FONT_FILE="${FONT_DIR}/SourceHanSans.ttc"
FONT_ZIP="${FONT_DIR}/SourceHanSans.ttc.zip"

SOURCE_HAN_SANS_URL="https://github.com/adobe-fonts/source-han-sans/releases/download/2.005R/01_SourceHanSans.ttc.zip"
SOURCE_HAN_SANS_SHA256="a024cf1759494847cd47aae4379bcb3dc530017c709f3f503ee0ed918dd92952"

mkdir -p "$FONT_DIR"

if [[ -f "$FONT_FILE" ]]; then
    echo "Already exists: $FONT_FILE"
    ls -lh "$FONT_FILE"
    exit 0
fi

command -v curl >/dev/null || {
    echo "Missing dependency: curl"
    exit 1
}

command -v unzip >/dev/null || {
    echo "Missing dependency: unzip"
    exit 1
}

command -v sha256sum >/dev/null || {
    echo "Missing dependency: sha256sum"
    exit 1
}

echo "Downloading Source Han Sans CJK font..."
curl -L "$SOURCE_HAN_SANS_URL" -o "$FONT_ZIP"

echo "Checking SHA256..."
echo "${SOURCE_HAN_SANS_SHA256}  ${FONT_ZIP}" | sha256sum -c -

echo "Extracting..."
unzip -o "$FONT_ZIP" -d "$FONT_DIR"

FOUND_FONT="$(find "$FONT_DIR" -type f -name 'SourceHanSans.ttc' | head -n 1)"

if [[ -z "$FOUND_FONT" ]]; then
    echo "Could not find SourceHanSans.ttc after extraction"
    exit 1
fi

if [[ "$FOUND_FONT" != "$FONT_FILE" ]]; then
    mv "$FOUND_FONT" "$FONT_FILE"
fi

rm -f "$FONT_ZIP"

echo "Ready:"
ls -lh "$FONT_FILE"