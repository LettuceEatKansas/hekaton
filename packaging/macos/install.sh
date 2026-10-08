#!/bin/bash
# Install winhands on this Mac: build (or take a prebuilt app), sign, copy to ~/Applications, register the
# LaunchAgent, walk through the two permissions, and print the MCP config. Safe to re-run to update.
# See --help and docs/INSTALL-mac.md.
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
Usage: install.sh [options]

Installs winhands.app into ~/Applications and registers its LaunchAgent. Run it again to update:
as long as the same signing identity is used, the macOS permissions are kept.

Options:
  --app PATH        Install this prebuilt winhands.app (for example from a CI download) instead of building.
                    It is re-signed with your local identity unless --keep-signature is given.
  --identity NAME   Sign with this certificate (Apple Development or Developer ID) instead of "$IDENTITY_NAME".
  --keep-signature  With --app: install the app exactly as it is, do not re-sign.
  --dest DIR        Install into DIR instead of ~/Applications.
  --no-open         Do not open System Settings or Finder.
  --no-launchd      Do not register the LaunchAgent.
  --yes             Answer yes to every question, wait for nothing.
  -h, --help        Show this text.
EOF
}

APP_SRC=""
IDENTITY=""
KEEP_SIG=0
NO_OPEN=0
NO_LAUNCHD=0

while [ $# -gt 0 ]; do
  case $1 in
    --app)
      [ $# -ge 2 ] || die "--app needs a value"
      APP_SRC=$2
      shift 2
      ;;
    --identity)
      [ $# -ge 2 ] || die "--identity needs a value"
      IDENTITY=$2
      shift 2
      ;;
    --dest)
      [ $# -ge 2 ] || die "--dest needs a value"
      DEST_DIR=$2
      shift 2
      ;;
    --keep-signature)
      KEEP_SIG=1
      shift
      ;;
    --no-open)
      NO_OPEN=1
      shift
      ;;
    --no-launchd)
      NO_LAUNCHD=1
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
[ "$UID_N" -ne 0 ] || die "Do not run this with sudo. winhands belongs to your own user account."
need launchctl "It ships with macOS; is this a normal Mac?"
need codesign "It ships with macOS; is this a normal Mac?"

DEST_APP="$DEST_DIR/$BIN_NAME.app"
DEST_BIN="$DEST_APP/Contents/MacOS/$BIN_NAME"
WORK=$(mktemp -d -t winhands-install)
trap 'rm -rf "$WORK"' EXIT

# --- 1. which signature ---------------------------------------------------------------------------------
SIGN_MODE=""
if [ -n "$IDENTITY" ]; then
  SIGN_MODE=named
elif [ "$KEEP_SIG" = 1 ]; then
  SIGN_MODE=keep
elif identity_available "$IDENTITY_NAME"; then
  SIGN_MODE=local
else
  info "No signing identity \"$IDENTITY_NAME\" yet."
  note "A stable identity is what lets macOS keep the winhands permissions across updates."
  if confirm "Create the one-time signing certificate now (recommended)?"; then
    "$PKG_DIR/make-signing-identity.sh" --yes
    if identity_available "$IDENTITY_NAME"; then SIGN_MODE=local; fi
  fi
  if [ -z "$SIGN_MODE" ]; then
    SIGN_MODE=adhoc
    warn "Continuing with an ad hoc signature: the permissions will reset every time you update."
  fi
fi

# --- 2. get the app -------------------------------------------------------------------------------------
STAGE="$WORK/$BIN_NAME.app"
if [ -n "$APP_SRC" ]; then
  [ -d "$APP_SRC" ] || die "--app: no such app bundle: $APP_SRC"
  info "Using prebuilt $APP_SRC"
  ditto "$APP_SRC" "$STAGE"
  xattr -dr com.apple.quarantine "$STAGE" 2>/dev/null || true
  case $SIGN_MODE in
    named)
      sign_app "$STAGE" "$IDENTITY" named
      ;;
    local)
      info "Re-signing with the local identity \"$IDENTITY_NAME\""
      sign_app "$STAGE" "$IDENTITY_NAME" local
      ;;
    adhoc)
      sign_app "$STAGE" "-" adhoc
      ;;
    keep) ;;
  esac
else
  build_args=(--out "$WORK/build")
  if [ "$SIGN_MODE" = named ]; then
    build_args+=(--identity "$IDENTITY")
  fi
  "$PKG_DIR/build-app.sh" "${build_args[@]}"
  ditto "$WORK/build/$BIN_NAME.app" "$STAGE"
fi
codesign --verify --strict --deep "$STAGE" || die "The app's signature is broken, not installing it."

# --- 3. install -----------------------------------------------------------------------------------------
if [ "$NO_LAUNCHD" = 0 ]; then
  info "Stopping any running winhands daemon"
  launchctl bootout "gui/$UID_N/$LABEL" >/dev/null 2>&1 || true
fi

info "Installing to $DEST_APP"
mkdir -p "$DEST_DIR"
rm -rf "$DEST_APP.new"
ditto "$STAGE" "$DEST_APP.new"
rm -rf "$DEST_APP"
mv "$DEST_APP.new" "$DEST_APP"
xattr -dr com.apple.quarantine "$DEST_APP" 2>/dev/null || true

# --- 4. LaunchAgent -------------------------------------------------------------------------------------
xml_escape() { sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'; }

if [ "$NO_LAUNCHD" = 0 ]; then
  info "Writing the LaunchAgent $PLIST"
  mkdir -p "$(dirname "$PLIST")" "$LOG_DIR"
  BIN_X=$(printf '%s' "$DEST_BIN" | xml_escape)
  LOG_X=$(printf '%s' "$LOG_DIR/daemon.log" | xml_escape)
  HOME_X=$(printf '%s' "$HOME" | xml_escape)
  # On demand, like the Windows scheduled task: loaded at login but not started (RunAtLoad false) and not
  # restarted when it exits (KeepAlive false). `winhands mcp` starts it with `launchctl kickstart`, which
  # makes launchd, not the caller, the parent, so macOS attributes the permissions to winhands.app and not
  # to Claude or Terminal. Aqua = only inside the logged-in graphical session, where the screen is visible.
  (
    umask 022
    cat >"$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>$LABEL</string>
	<key>AssociatedBundleIdentifiers</key>
	<array>
		<string>$APP_ID</string>
	</array>
	<key>ProgramArguments</key>
	<array>
		<string>$BIN_X</string>
		<string>daemon</string>
	</array>
	<key>RunAtLoad</key>
	<false/>
	<key>KeepAlive</key>
	<false/>
	<key>LimitLoadToSessionType</key>
	<string>Aqua</string>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>WorkingDirectory</key>
	<string>$HOME_X</string>
	<key>StandardOutPath</key>
	<string>$LOG_X</string>
	<key>StandardErrorPath</key>
	<string>$LOG_X</string>
</dict>
</plist>
EOF
  )
  plutil -lint "$PLIST" >/dev/null || die "Wrote an invalid plist: $PLIST"

  info "Registering it with launchd (gui/$UID_N)"
  if launchctl bootstrap "gui/$UID_N" "$PLIST" 2>"$WORK/bootstrap.err"; then
    if launchctl print "gui/$UID_N/$LABEL" >/dev/null 2>&1; then
      note "Loaded. It starts on demand when an AI client uses winhands."
    fi
  else
    warn "launchctl bootstrap failed: $(tr '\n' ' ' <"$WORK/bootstrap.err")"
    warn "The app is installed. If you are on an SSH session, log in at the Mac itself and run this script again."
  fi
fi

echo
info "Signature of the installed app"
codesign --display --verbose=2 --requirements - "$DEST_APP" 2>&1 | grep -E '^(Identifier|Authority|Signature|# designated)|designated' | sed 's/^/    /' || true

# --- 5. permissions -------------------------------------------------------------------------------------
echo
info "Two permissions to switch on (macOS asks you to do this yourself, once)"
cat <<EOF
    1. Accessibility: lets winhands read what is on screen as buttons and text, and click and type
       for the AI agent.
    2. Screen Recording: lets winhands take screenshots so the agent can see.

    In each list, find "winhands" and switch it ON. If winhands is not in the list yet, click the
    "+" button, pick $DEST_APP, and click Open. (A Finder window showing the app opens
    too, so you can drag it into the list.) macOS may ask you to quit and reopen winhands: choose
    "Later". There is nothing to restart, winhands starts fresh the next time it is used.
EOF

if [ "$NO_OPEN" = 0 ]; then
  # `doctor --prompt`, run as the app, makes macOS list winhands in both panes and may pop up its own
  # "winhands would like to ..." boxes: choose "Open System Settings" there.
  info "Asking macOS to list winhands under Accessibility and Screen Recording"
  run_as_app "$DEST_APP" doctor --prompt >/dev/null || note "(could not trigger the prompts; use the + button in each list)"
  open -R "$DEST_APP" || true
  echo
  info "Opening System Settings: Accessibility"
  open "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility" || true
  pause_for_user "    Switch winhands ON, then press Enter here to continue. "
  info "Opening System Settings: Screen Recording"
  open "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture" || true
  pause_for_user "    Switch winhands ON, then press Enter here to continue. "
fi

# --- 6. doctor ------------------------------------------------------------------------------------------
echo
info "winhands doctor (permission status, checked as winhands.app itself)"
if ! run_as_app "$DEST_APP" doctor | sed 's/^/    /'; then
  note "(could not run it as the app; running it from this Terminal instead, which can show"
  note " Terminal's permissions rather than winhands's)"
  "$DEST_BIN" doctor 2>&1 | sed 's/^/    /' || true
fi

# --- 7. next steps --------------------------------------------------------------------------------------
cat <<EOF

==> Last steps

  A. Acknowledge the safety notice once. In Terminal:

         "$DEST_BIN" acknowledge

     (Optional shortcut: add  alias winhands='$DEST_BIN'  to ~/.zshrc, then just type: winhands acknowledge)

  B. Tell your AI client about winhands.

     Claude Code:

         claude mcp add winhands -- "$DEST_BIN" mcp

     Claude Desktop: open the file below (create it if missing) and add the "winhands" entry inside
     "mcpServers", then quit and reopen Claude Desktop:

         ~/Library/Application Support/Claude/claude_desktop_config.json

         {
           "mcpServers": {
             "winhands": {
               "command": "$DEST_BIN",
               "args": ["mcp"]
             }
           }
         }

  Logs: $LOG_DIR/daemon.log      Remove everything: packaging/macos/uninstall.sh
EOF
