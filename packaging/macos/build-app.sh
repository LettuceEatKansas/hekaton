#!/bin/bash
# Build winhands.app: a universal (arm64 + x86_64) release binary in a signed macOS app bundle.
# Run on a Mac. See --help.
set -euo pipefail

# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

usage() {
  cat <<EOF
Usage: build-app.sh [options]

Builds winhands.app (bundle id $APP_ID) from the winhands binary and signs it.

Options:
  --identity NAME   Sign with this code-signing certificate (an Apple Development or Developer ID
                    certificate), with the hardened runtime and a secure timestamp.
  --binary PATH     Use this already-built winhands executable instead of running cargo. It is copied
                    in as is, so pass a universal binary if you want a universal app.
  --out DIR         Where winhands.app is written (default: target/macos in the repo).
  --version X.Y.Z   Version recorded in Info.plist (default: read from Cargo).
  --native          Build only for this Mac's CPU. Faster, for development.
  -h, --help        Show this text.

Which signature is used, in this order:
  1. --identity NAME, if given.
  2. The self-signed "$IDENTITY_NAME" identity in your login keychain, if it exists
     (create it once with packaging/macos/make-signing-identity.sh).
  3. Ad hoc. This works, but macOS forgets the Accessibility and Screen Recording permissions
     every time the app is rebuilt, because an ad hoc signature is different on every build.
EOF
}

IDENTITY=""
BINARY=""
OUT_DIR="$REPO_ROOT/target/macos"
VERSION=""
NATIVE=0

while [ $# -gt 0 ]; do
  case $1 in
    --identity)
      [ $# -ge 2 ] || die "--identity needs a value"
      IDENTITY=$2
      shift 2
      ;;
    --binary)
      [ $# -ge 2 ] || die "--binary needs a value"
      BINARY=$2
      shift 2
      ;;
    --out)
      [ $# -ge 2 ] || die "--out needs a value"
      OUT_DIR=$2
      shift 2
      ;;
    --version)
      [ $# -ge 2 ] || die "--version needs a value"
      VERSION=$2
      shift 2
      ;;
    --native)
      NATIVE=1
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
need codesign "It ships with macOS; is this a normal Mac?"
need plutil "It ships with macOS; is this a normal Mac?"
need lipo "Install the Xcode command line tools: xcode-select --install"

WORK=$(mktemp -d -t winhands-build)
trap 'rm -rf "$WORK"' EXIT

# --- 1. the executable ----------------------------------------------------------------------------------
build_binary() {
  need cargo "Install Rust from https://rustup.rs, then open a new Terminal window."
  need rustup "Install Rust from https://rustup.rs (Homebrew's 'rust' lacks the second CPU target)."
  local host targets t target_dir
  local -a outs
  # 2026-10-11: Homebrew's cargo ahead of rustup's in PATH built without the x86_64 target that
  # rustup had installed ("can't find crate for core"). Use rustup's toolchain first.
  PATH="$(dirname "$(rustup which cargo)"):$PATH"
  export PATH
  host=$(rustc -vV | sed -n 's/^host: //p')
  if [ "$NATIVE" = 1 ]; then
    targets=$host
  else
    targets="aarch64-apple-darwin x86_64-apple-darwin"
  fi
  target_dir="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
  outs=()
  for t in $targets; do
    if ! rustup target list --installed | grep -x "$t" >/dev/null; then
      info "rustup target add $t"
      rustup target add "$t"
    fi
    info "cargo build --release -p winhands --target $t"
    (cd "$REPO_ROOT" && MACOSX_DEPLOYMENT_TARGET="$MIN_MACOS" cargo build --release --locked -p winhands --target "$t")
    outs+=("$target_dir/$t/release/$BIN_NAME")
  done
  if [ "${#outs[@]}" -eq 1 ]; then
    cp "${outs[0]}" "$WORK/$BIN_NAME"
  else
    info "lipo: joining both CPU builds into one universal binary"
    lipo -create "${outs[@]}" -output "$WORK/$BIN_NAME"
  fi
}

if [ -n "$BINARY" ]; then
  [ -f "$BINARY" ] || die "--binary: no such file: $BINARY"
  info "Using prebuilt binary $BINARY"
  cp "$BINARY" "$WORK/$BIN_NAME"
else
  build_binary
fi
chmod 755 "$WORK/$BIN_NAME"
lipo -info "$WORK/$BIN_NAME" >/dev/null 2>&1 || die "That file is not a macOS executable: ${BINARY:-built binary}"
ARCHS=$(lipo -archs "$WORK/$BIN_NAME")
info "Executable architectures: $ARCHS"
case $ARCHS in
  *arm64*x86_64* | *x86_64*arm64*) ;;
  *) warn "The executable is not universal ($ARCHS). The app will only run on that CPU type." ;;
esac

# --- 2. the bundle --------------------------------------------------------------------------------------
[ -n "$VERSION" ] || VERSION=$(package_version)
case $VERSION in
  [0-9]*.[0-9]*.[0-9]*) ;;
  *) die "Could not work out the version (got '$VERSION'). Pass --version X.Y.Z." ;;
esac

APP="$OUT_DIR/$BIN_NAME.app"
info "Assembling $APP (version $VERSION)"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$WORK/$BIN_NAME" "$APP/Contents/MacOS/$BIN_NAME"
printf 'APPL????' >"$APP/Contents/PkgInfo"

# LSUIElement: no Dock icon and no app menu, but the app may still show a menu-bar item.
# No NSAppleEventsUsageDescription: winhands does not send Apple Events (it posts CGEvents and reads the
# Accessibility tree). macOS has no Info.plist key for the Accessibility or Screen Recording prompts.
cat >"$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleExecutable</key>
	<string>$BIN_NAME</string>
	<key>CFBundleIdentifier</key>
	<string>$APP_ID</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>$BIN_NAME</string>
	<key>CFBundleDisplayName</key>
	<string>$BIN_NAME</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$VERSION</string>
	<key>CFBundleVersion</key>
	<string>$VERSION</string>
	<key>CFBundleSupportedPlatforms</key>
	<array>
		<string>MacOSX</string>
	</array>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.developer-tools</string>
	<key>LSMinimumSystemVersion</key>
	<string>$MIN_MACOS</string>
	<key>LSUIElement</key>
	<true/>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSHumanReadableCopyright</key>
	<string>MIT License</string>
</dict>
</plist>
EOF
plutil -lint "$APP/Contents/Info.plist" >/dev/null

# --- 3. sign --------------------------------------------------------------------------------------------
if [ -n "$IDENTITY" ]; then
  identity_available "$IDENTITY" || die "No code-signing identity matching '$IDENTITY' in your keychains. See: security find-identity -p codesigning"
  info "Signing with '$IDENTITY' (hardened runtime, secure timestamp)"
  sign_app "$APP" "$IDENTITY" named
  SIGNED_AS="certificate '$IDENTITY'"
elif identity_available "$IDENTITY_NAME"; then
  info "Signing with the local identity '$IDENTITY_NAME' (hardened runtime)"
  sign_app "$APP" "$IDENTITY_NAME" local
  SIGNED_AS="local identity '$IDENTITY_NAME'"
else
  cat >&2 <<EOF

##########################################################################################
 AD HOC SIGNATURE: macOS will forget the winhands permissions on every rebuild.

 This app is signed ad hoc because no signing identity was found. An ad hoc signature is
 different on every build, and macOS ties Accessibility and Screen Recording permission to
 the signature, so you would have to switch the two permissions on again after each update.

 Fix it once (about a minute):  packaging/macos/make-signing-identity.sh
 then run this script again. Rebuilds then keep the same identity and the same permissions.
##########################################################################################

EOF
  sign_app "$APP" "-" adhoc
  SIGNED_AS="ad hoc (permissions will reset on every rebuild)"
fi

info "Done: $APP"
note "version:   $VERSION"
note "CPUs:      $ARCHS"
note "signed as: $SIGNED_AS"
codesign --display --requirements - "$APP" 2>&1 | sed 's/^/    /' || true
