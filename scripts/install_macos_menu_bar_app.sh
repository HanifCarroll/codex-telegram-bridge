#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="Codex Bridge"
BUNDLE_ID="com.hanifcarroll.codex-telegram-bridge.menubar"
APP_EXECUTABLE="CodexBridgeMenuBar"
BUILD_SCRIPT="$ROOT/scripts/build_macos_menu_bar_app.sh"
SOURCE_APP="$ROOT/target/macos-menu-bar/$APP_NAME.app"
INSTALL_DIR="${CODEX_BRIDGE_INSTALL_DIR:-$HOME/Applications}"
OPEN_APP=1
REGISTER_LOGIN_ITEM=1

usage() {
    cat <<USAGE
Usage: scripts/install_macos_menu_bar_app.sh [options]

Build and install the Codex Bridge menu bar app.

Options:
  --install-dir <path>  Install app bundle into this directory. Defaults to ~/Applications.
  --no-login-item       Do not register the app to open at login.
  --no-open             Do not launch the installed app after copying it.
  -h, --help            Show this help.
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --install-dir)
            if [[ $# -lt 2 ]]; then
                echo "error: --install-dir requires a path" >&2
                exit 2
            fi
            INSTALL_DIR="$2"
            shift 2
            ;;
        --no-login-item)
            REGISTER_LOGIN_ITEM=0
            shift
            ;;
        --no-open)
            OPEN_APP=0
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

INSTALL_APP="$INSTALL_DIR/$APP_NAME.app"

"$BUILD_SCRIPT"

mkdir -p "$INSTALL_DIR"

if pgrep -x "$APP_EXECUTABLE" >/dev/null; then
    /usr/bin/osascript -e "tell application id \"$BUNDLE_ID\" to quit" >/dev/null 2>&1 || true
    sleep 1
    pkill -x "$APP_EXECUTABLE" >/dev/null 2>&1 || true
fi

rm -rf "$INSTALL_APP"
ditto "$SOURCE_APP" "$INSTALL_APP"
/usr/bin/plutil -lint "$INSTALL_APP/Contents/Info.plist" >/dev/null

if [[ "$REGISTER_LOGIN_ITEM" -eq 1 ]]; then
    INSTALL_APP="$INSTALL_APP" APP_NAME="$APP_NAME" /usr/bin/osascript >/dev/null <<'APPLESCRIPT'
set appPath to system attribute "INSTALL_APP"
set appName to system attribute "APP_NAME"

tell application "System Events"
    set matchingItems to login items whose name is appName
    repeat with itemRef in matchingItems
        delete itemRef
    end repeat
    make login item at end with properties {path:appPath, hidden:false, name:appName}
end tell
APPLESCRIPT
fi

if [[ "$OPEN_APP" -eq 1 ]]; then
    open "$INSTALL_APP"
fi

echo "Installed $INSTALL_APP"

if [[ "$REGISTER_LOGIN_ITEM" -eq 1 ]]; then
    echo "Registered $APP_NAME as a Login Item"
else
    echo "Skipped Login Item registration"
fi

if [[ "$OPEN_APP" -eq 1 ]]; then
    echo "Launched $APP_NAME"
else
    echo "Skipped launch"
fi
