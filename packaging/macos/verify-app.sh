#!/bin/bash
# Check a built winhands.app: signature intact, Info.plist sane, both CPU types present, binary runs.
# Usage: verify-app.sh [path/to/winhands.app]   (default: target/macos/winhands.app)
set -euo pipefail

# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

APP="${1:-$REPO_ROOT/target/macos/$BIN_NAME.app}"
BIN="$APP/Contents/MacOS/$BIN_NAME"
PLIST="$APP/Contents/Info.plist"
FAILS=0

require_macos
[ -d "$APP" ] || die "No such app bundle: $APP"

ok() { printf '  ok    %s\n' "$*"; }
bad() {
  printf '  FAIL  %s\n' "$*" >&2
  FAILS=$((FAILS + 1))
}

plist_get() { /usr/libexec/PlistBuddy -c "Print :$1" "$PLIST" 2>/dev/null || true; }

info "Verifying $APP"

if codesign --verify --strict --deep --verbose=2 "$APP" 2>&1 | sed 's/^/        /'; then
  ok "codesign --verify --strict --deep"
else
  bad "codesign verification"
fi

if plutil -lint "$PLIST" >/dev/null; then ok "plutil -lint Info.plist"; else bad "plutil -lint Info.plist"; fi

expect() { # expect LABEL ACTUAL EXPECTED
  if [ "$2" = "$3" ]; then ok "$1 = $3"; else bad "$1 is '$2', expected '$3'"; fi
}
expect "bundle id" "$(plist_get CFBundleIdentifier)" "$APP_ID"
expect "CFBundleExecutable" "$(plist_get CFBundleExecutable)" "$BIN_NAME"
expect "LSUIElement (no Dock icon)" "$(plist_get LSUIElement)" "true"
if [ -n "$(plist_get CFBundleShortVersionString)" ]; then ok "version $(plist_get CFBundleShortVersionString)"; else bad "no CFBundleShortVersionString"; fi

[ -x "$BIN" ] || die "Missing executable: $BIN"
LIPO=$(lipo -info "$BIN" 2>&1)
printf '        %s\n' "$LIPO"
case $LIPO in
  *arm64*) ok "arm64 slice" ;;
  *) bad "no arm64 slice" ;;
esac
case $LIPO in
  *x86_64*) ok "x86_64 slice" ;;
  *) bad "no x86_64 slice" ;;
esac

codesign_id=$(codesign --display --verbose=2 "$APP" 2>&1 | sed -n 's/^Identifier=//p')
expect "signed identifier" "$codesign_id" "$APP_ID"

# Run each CPU slice. The slice that is not native needs Rosetta; skip it with a note if Rosetta is absent.
run_slice() {
  local arch=$1
  if ! arch "-$arch" /usr/bin/true 2>/dev/null; then
    printf '  skip  %s slice not run (this Mac cannot run %s code, Rosetta missing?)\n' "$arch" "$arch"
    return 0
  fi
  if arch "-$arch" "$BIN" --version >/dev/null 2>&1 || arch "-$arch" "$BIN" --help >/dev/null 2>&1; then
    ok "$arch slice runs (--version or --help)"
  else
    bad "$arch slice failed to run --version and --help"
  fi
}
run_slice arm64
run_slice x86_64

if [ "$FAILS" -gt 0 ]; then
  die "$FAILS check(s) failed"
fi
info "All checks passed"
