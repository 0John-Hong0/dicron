#!/usr/bin/env bash
set -euo pipefail

APP_NAME="Dicron"
BINARY_NAME="dicron"
PACKAGE_NAME="dicron"
BUNDLE_IDENTIFIER="${BUNDLE_IDENTIFIER:-io.github.0johnhong0.dicron}"
PACKAGE_SUFFIX="${MACOS_PACKAGE_SUFFIX:-macos-$(uname -m)}"
DIST_DIR="dist/macos"
APP_DIR="$DIST_DIR/$APP_NAME.app"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"
INFO_PLIST_TEMPLATE="packaging/macos/Info.plist"
ICON_SOURCE="assets/icon.png"
ICONSET_DIR="$DIST_DIR/icon.iconset"
DMG_ROOT="$DIST_DIR/dmg-root"
BACKGROUND_DIR="$DMG_ROOT/.background"
BACKGROUND_IMAGE="$BACKGROUND_DIR/background.png"
BACKGROUND_SWIFT="$DIST_DIR/make-dmg-background.swift"
MOUNT_DIR=""

if [[ ! -f Cargo.toml ]]; then
    echo "ERROR: Run this from the project root where Cargo.toml exists."
    exit 1
fi

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "ERROR: macOS packaging requires macOS."
    exit 1
fi

for command_name in cargo hdiutil iconutil sips swift; do
    if ! command -v "$command_name" >/dev/null 2>&1; then
        echo "ERROR: $command_name not found."
        exit 1
    fi
done

APP_VERSION="$(
    cargo metadata --no-deps --format-version 1 \
        | python3 -c 'import json, sys; print(json.load(sys.stdin)["packages"][0]["version"])'
)"
BINARY_PATH="target/release/$BINARY_NAME"
DMG_PATH="$DIST_DIR/${PACKAGE_NAME}-${APP_VERSION}-${PACKAGE_SUFFIX}.dmg"
VOLUME_NAME="$APP_NAME $APP_VERSION"
RW_DMG_PATH="$DIST_DIR/${PACKAGE_NAME}-${APP_VERSION}-${PACKAGE_SUFFIX}.rw.dmg"
MOUNT_DIR="/Volumes/$VOLUME_NAME"

if [[ ! -x "$BINARY_PATH" ]]; then
    echo "ERROR: macOS binary not found. Run cargo build --release --locked first."
    exit 1
fi

cleanup() {
    if [[ -n "$MOUNT_DIR" && -d "$MOUNT_DIR" ]]; then
        hdiutil detach "$MOUNT_DIR" -quiet >/dev/null 2>&1 || true
    fi

    rm -rf "$RW_DMG_PATH"
}

trap cleanup EXIT

rm -rf "$APP_DIR" "$ICONSET_DIR" "$DMG_ROOT" "$DMG_PATH" "$RW_DMG_PATH" "$BACKGROUND_SWIFT"
mkdir -p "$MACOS_DIR" "$RESOURCES_DIR" "$ICONSET_DIR" "$DMG_ROOT" "$BACKGROUND_DIR"

cp "$BINARY_PATH" "$MACOS_DIR/$BINARY_NAME"
chmod 755 "$MACOS_DIR/$BINARY_NAME"

sed \
    -e "s/__APP_VERSION__/$APP_VERSION/g" \
    -e "s/__BUNDLE_IDENTIFIER__/$BUNDLE_IDENTIFIER/g" \
    "$INFO_PLIST_TEMPLATE" > "$CONTENTS_DIR/Info.plist"

cat > "$CONTENTS_DIR/PkgInfo" <<'EOF'
APPL????
EOF

sips -z 16 16 "$ICON_SOURCE" --out "$ICONSET_DIR/icon_16x16.png" >/dev/null
sips -z 32 32 "$ICON_SOURCE" --out "$ICONSET_DIR/icon_16x16@2x.png" >/dev/null
sips -z 32 32 "$ICON_SOURCE" --out "$ICONSET_DIR/icon_32x32.png" >/dev/null
sips -z 64 64 "$ICON_SOURCE" --out "$ICONSET_DIR/icon_32x32@2x.png" >/dev/null
sips -z 128 128 "$ICON_SOURCE" --out "$ICONSET_DIR/icon_128x128.png" >/dev/null
sips -z 256 256 "$ICON_SOURCE" --out "$ICONSET_DIR/icon_128x128@2x.png" >/dev/null
sips -z 256 256 "$ICON_SOURCE" --out "$ICONSET_DIR/icon_256x256.png" >/dev/null
sips -z 512 512 "$ICON_SOURCE" --out "$ICONSET_DIR/icon_256x256@2x.png" >/dev/null
sips -z 512 512 "$ICON_SOURCE" --out "$ICONSET_DIR/icon_512x512.png" >/dev/null
sips -z 1024 1024 "$ICON_SOURCE" --out "$ICONSET_DIR/icon_512x512@2x.png" >/dev/null
iconutil -c icns "$ICONSET_DIR" -o "$RESOURCES_DIR/icon.icns"

if [[ -d assets/licenses ]]; then
    mkdir -p "$RESOURCES_DIR/licenses"
    cp assets/licenses/* "$RESOURCES_DIR/licenses/"
fi

cat > "$BACKGROUND_SWIFT" <<'SWIFT'
import AppKit

let outputPath = CommandLine.arguments[1]
let canvasSize = NSSize(width: 1600, height: 1200)
let image = NSImage(size: canvasSize)

func color(_ red: CGFloat, _ green: CGFloat, _ blue: CGFloat, _ alpha: CGFloat = 1.0) -> NSColor {
    NSColor(calibratedRed: red / 255.0, green: green / 255.0, blue: blue / 255.0, alpha: alpha)
}

func drawText(_ text: String, in rect: NSRect, size: CGFloat, weight: NSFont.Weight, color textColor: NSColor, alignment: NSTextAlignment = .center) {
    let paragraph = NSMutableParagraphStyle()
    paragraph.alignment = alignment

    let attributes: [NSAttributedString.Key: Any] = [
        .font: NSFont.systemFont(ofSize: size, weight: weight),
        .foregroundColor: textColor,
        .paragraphStyle: paragraph,
    ]

    NSString(string: text).draw(in: rect, withAttributes: attributes)
}

image.lockFocus()

color(31, 34, 38).setFill()
NSBezierPath(rect: NSRect(origin: .zero, size: canvasSize)).fill()

color(42, 47, 54).setFill()
NSBezierPath(roundedRect: NSRect(x: 34, y: 834, width: 572, height: 332), xRadius: 24, yRadius: 24).fill()

color(58, 122, 189, 0.20).setFill()
NSBezierPath(roundedRect: NSRect(x: 58, y: 1038, width: 524, height: 74), xRadius: 18, yRadius: 18).fill()

drawText(
    "Install Dicron",
    in: NSRect(x: 72, y: 1068, width: 496, height: 26),
    size: 21,
    weight: .semibold,
    color: color(242, 245, 248)
)

drawText(
    "Drag the app into Applications",
    in: NSRect(x: 72, y: 1046, width: 496, height: 20),
    size: 13,
    weight: .medium,
    color: color(178, 188, 200)
)

let arrow = NSBezierPath()
arrow.move(to: NSPoint(x: 258, y: 986))
arrow.line(to: NSPoint(x: 374, y: 986))
arrow.lineWidth = 7
arrow.lineCapStyle = .round
color(90, 178, 255).setStroke()
arrow.stroke()

let arrowHead = NSBezierPath()
arrowHead.move(to: NSPoint(x: 374, y: 986))
arrowHead.line(to: NSPoint(x: 350, y: 1002))
arrowHead.line(to: NSPoint(x: 350, y: 970))
arrowHead.close()
color(90, 178, 255).setFill()
arrowHead.fill()

drawText(
    "Open from Applications after copying.",
    in: NSRect(x: 72, y: 872, width: 496, height: 18),
    size: 12,
    weight: .regular,
    color: color(138, 148, 160)
)

image.unlockFocus()

guard
    let tiffData = image.tiffRepresentation,
    let bitmap = NSBitmapImageRep(data: tiffData),
    let pngData = bitmap.representation(using: .png, properties: [:])
else {
    fatalError("failed to render DMG background")
}

try pngData.write(to: URL(fileURLWithPath: outputPath))
SWIFT

swift "$BACKGROUND_SWIFT" "$BACKGROUND_IMAGE"
rm -f "$BACKGROUND_SWIFT"

cp -R "$APP_DIR" "$DMG_ROOT/"
ln -s /Applications "$DMG_ROOT/Applications"

hdiutil create \
    -volname "$APP_NAME $APP_VERSION" \
    -srcfolder "$DMG_ROOT" \
    -ov \
    -format UDRW \
    "$RW_DMG_PATH"

if [[ -d "$MOUNT_DIR" ]]; then
    hdiutil detach "$MOUNT_DIR" -quiet >/dev/null 2>&1 || true
fi

hdiutil attach "$RW_DMG_PATH" -readwrite -noverify -noautoopen

osascript <<EOF
set background_file to POSIX file "$MOUNT_DIR/.background/background.png" as alias

tell application "Finder"
  tell disk "$VOLUME_NAME"
    open
    set current view of container window to icon view
    set toolbar visible of container window to false
    set statusbar visible of container window to false
    set bounds of container window to {240, 160, 880, 560}

    set view_options to icon view options of container window
    set arrangement of view_options to not arranged
    set background picture of view_options to background_file
    set icon size of view_options to 96
    set label position of view_options to bottom
    set text size of view_options to 13

    set position of item "$APP_NAME.app" of container window to {188, 214}
    set position of item "Applications" of container window to {452, 214}

    update without registering applications
    delay 1
    close
  end tell
end tell
EOF

sync
hdiutil detach "$MOUNT_DIR" -quiet

hdiutil convert "$RW_DMG_PATH" \
    -format UDZO \
    -imagekey zlib-level=9 \
    -o "$DMG_PATH"

rm -f "$RW_DMG_PATH"

echo
echo "DONE:"
echo "  $DMG_PATH"
