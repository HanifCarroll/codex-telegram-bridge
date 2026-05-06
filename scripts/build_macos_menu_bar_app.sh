#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="Codex Bridge"
APP_DIR="$ROOT/target/macos-menu-bar/$APP_NAME.app"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"
SWIFT_PACKAGE="$ROOT/apps/macos-menu-bar"
SWIFT_BINARY="$SWIFT_PACKAGE/.build/release/CodexBridgeMenuBar"
BRIDGE_BINARY="$ROOT/target/release/codex-telegram-bridge"

cargo build --release --manifest-path "$ROOT/Cargo.toml"
swift build --package-path "$SWIFT_PACKAGE" -c release

rm -rf "$APP_DIR"
mkdir -p "$MACOS_DIR" "$RESOURCES_DIR"

cp "$SWIFT_BINARY" "$MACOS_DIR/CodexBridgeMenuBar"
cp "$BRIDGE_BINARY" "$MACOS_DIR/codex-telegram-bridge"

{
    printf '%s\n' '<?xml version="1.0" encoding="UTF-8"?>'
    printf '%s\n' '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">'
    printf '%s\n' '<plist version="1.0">'
    printf '%s\n' '<dict>'
    printf '%s\n' '    <key>CFBundleDevelopmentRegion</key>'
    printf '%s\n' '    <string>en</string>'
    printf '%s\n' '    <key>CFBundleExecutable</key>'
    printf '%s\n' '    <string>CodexBridgeMenuBar</string>'
    printf '%s\n' '    <key>CFBundleIdentifier</key>'
    printf '%s\n' '    <string>com.hanifcarroll.codex-telegram-bridge.menubar</string>'
    printf '%s\n' '    <key>CFBundleInfoDictionaryVersion</key>'
    printf '%s\n' '    <string>6.0</string>'
    printf '%s\n' '    <key>CFBundleName</key>'
    printf '%s\n' '    <string>Codex Bridge</string>'
    printf '%s\n' '    <key>CFBundlePackageType</key>'
    printf '%s\n' '    <string>APPL</string>'
    printf '%s\n' '    <key>CFBundleShortVersionString</key>'
    printf '%s\n' '    <string>0.1.0</string>'
    printf '%s\n' '    <key>CFBundleVersion</key>'
    printf '%s\n' '    <string>1</string>'
    printf '%s\n' '    <key>LSMinimumSystemVersion</key>'
    printf '%s\n' '    <string>13.0</string>'
    printf '%s\n' '    <key>LSUIElement</key>'
    printf '%s\n' '    <true/>'
    printf '%s\n' '    <key>NSHighResolutionCapable</key>'
    printf '%s\n' '    <true/>'
    printf '%s\n' '</dict>'
    printf '%s\n' '</plist>'
} > "$CONTENTS_DIR/Info.plist"

echo "$APP_DIR"
