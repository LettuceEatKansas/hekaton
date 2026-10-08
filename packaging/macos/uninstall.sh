#!/bin/bash
# Remove winhands from this Mac: stop and unregister the LaunchAgent, delete the plist and the app.
# The Accessibility and Screen Recording entries and the signing certificate are left alone unless you ask.
set -euo pipefail

# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

LABEL="$APP_ID"
UID_N=$(id -u)
DEST_DIR="$HOME/Applications"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOG_DIR="$HOME/Library/Logs/winhands"

usage() {
  cat <<EOF
Usage: uninstall.sh [options]

Stops winhands, removes ~/Library/LaunchAgents/$LABEL.plist and ~/Applications/winhands.app.

Options:
  --dest DIR             The folder winhands.app was installed into (default: ~/Applications).
  --reset-permissions    Also remove winhands's Accessibility and Screen Recording entries (tccutil).
  --purge-logs           Also delete $LOG_DIR.
  --yes                  Do not ask for confirmation.
  -h, --help             Show this text.

Left in place by default:
  - The permission entries. macOS keeps them after the app is gone. Remove them with --reset-permissions
    or by hand:  tccutil reset Accessibility $APP_ID
                 tccutil reset ScreenCapture $APP_ID
    (or System Settings > Privacy & Security > Accessibility / Screen Recording, select winhands, click "-").
  - The signing certificate "$IDENTITY_NAME" in your login keychain. Delete it with:
                 security delete-identity -c $IDENTITY_NAME
EOF
}

RESET=0
PURGE=0
while [ $# -gt 0 ]; do
  case $1 in
    --dest)
      [ $# -ge 2 ] || die "--dest needs a value"
      DEST_DIR=$2
      shift 2
      ;;
    --reset-permissions)
      RESET=1
      shift
      ;;
    --purge-logs)
      PURGE=1
      shift
      ;;
    --yes)
      WH_YES=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      die "Unknown option: $1"
      ;;
  esac
done

require_macos
DEST_APP="$DEST_DIR/$BIN_NAME.app"

if [ ! -e "$DEST_APP" ] && [ ! -e "$PLIST" ]; then
  info "winhands does not appear to be installed (no $DEST_APP, no $PLIST)."
else
  confirm "Remove winhands ($DEST_APP and its LaunchAgent)?" || die "Cancelled."
fi

info "Stopping and unregistering the LaunchAgent"
launchctl bootout "gui/$UID_N/$LABEL" >/dev/null 2>&1 || true
rm -f "$PLIST"

info "Removing $DEST_APP"
rm -rf "$DEST_APP"

if [ "$PURGE" = 1 ]; then
  info "Removing $LOG_DIR"
  rm -rf "$LOG_DIR"
fi

if [ "$RESET" = 1 ]; then
  info "Removing the permission entries"
  tccutil reset Accessibility "$APP_ID" || true
  tccutil reset ScreenCapture "$APP_ID" || true
fi

TCC_NOTE=""
if [ "$RESET" = 0 ]; then
  TCC_NOTE="    - The Accessibility and Screen Recording entries for winhands. They do no harm. To remove them:
          tccutil reset Accessibility $APP_ID
          tccutil reset ScreenCapture $APP_ID"
fi

echo
info "winhands is removed."
echo "  Still on this Mac, on purpose:"
if [ -n "$TCC_NOTE" ]; then echo "$TCC_NOTE"; fi
cat <<EOF
    - The signing certificate "$IDENTITY_NAME" (keeps permissions stable if you install again). To delete it:
          security delete-identity -c $IDENTITY_NAME
    - Your MCP config: run  claude mcp remove winhands  and delete the "winhands" entry from
      ~/Library/Application Support/Claude/claude_desktop_config.json if you added one.
EOF
