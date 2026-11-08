#!/bin/bash
# Shared helpers for the hekaton macOS packaging scripts. Source this file, do not run it.
# Written for the bash 3.2 that ships with macOS (no associative arrays, no mapfile, no ${var,,}).
# shellcheck shell=bash disable=SC2034

APP_ID="org.hekaton.app"
BIN_NAME="hekaton"
APP_DISPLAY_NAME="Hekaton"   # what System Settings and the Dock show
IDENTITY_NAME="hekaton-local"
# ScreenCaptureKit screenshots (SCScreenshotManager) need macOS 14.
MIN_MACOS="14.0"

PKG_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$PKG_DIR/../.." && pwd)

# Test hooks. HEKATON_KEYCHAIN points the signing scripts at a different keychain (CI uses a throwaway
# one); unset means the user's login keychain. HEKATON_YES=1 answers "yes" to every question.
HK_KEYCHAIN="${HEKATON_KEYCHAIN:-}"
HK_YES="${HEKATON_YES:-0}"

info() { printf '==> %s\n' "$*"; }
note() { printf '    %s\n' "$*"; }
warn() { printf 'WARNING: %s\n' "$*" >&2; }
die() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

require_macos() {
  [ "$(uname -s)" = "Darwin" ] || die "This script only runs on macOS."
}

need() {
  command -v "$1" >/dev/null 2>&1 || die "'$1' not found. $2"
}

# Ask a yes/no question, default yes. HEKATON_YES=1 (or --yes) answers yes. With no terminal attached
# and no --yes, the answer is no, so nothing is ever created behind the user's back.
confirm() {
  local ans=""
  if [ "$HK_YES" = 1 ]; then
    return 0
  fi
  if [ ! -t 0 ]; then
    return 1
  fi
  printf '%s [Y/n] ' "$1"
  read -r ans || true
  case $ans in
    n | N | no | NO | No) return 1 ;;
    *) return 0 ;;
  esac
}

# Wait for Enter so the user can finish a manual step. Skipped when --yes or no terminal.
pause_for_user() {
  if [ "$HK_YES" = 1 ] || [ ! -t 0 ]; then
    return 0
  fi
  printf '%s' "$1"
  read -r _ || true
}

# run_with_timeout SECONDS CMD...  (macOS has no timeout(1)). Returns the command's status, 124 on timeout.
run_with_timeout() {
  local secs=$1 pid watcher rc=0
  shift
  "$@" &
  pid=$!
  (
    sleep "$secs"
    kill "$pid" 2>/dev/null
  ) >/dev/null 2>&1 &
  watcher=$!
  wait "$pid" || rc=$?
  if kill -0 "$watcher" 2>/dev/null; then
    kill "$watcher" 2>/dev/null || true
  else
    rc=124
  fi
  wait "$watcher" 2>/dev/null || true
  return "$rc"
}

# Absolute path of the keychain the scripts work in. Never a system keychain.
login_keychain() {
  local p="$HK_KEYCHAIN"
  if [ -z "$p" ]; then
    p=$(security login-keychain 2>/dev/null | sed -e 's/^[[:space:]]*"//' -e 's/"[[:space:]]*$//') || p=""
    [ -n "$p" ] || p="$HOME/Library/Keychains/login.keychain-db"
  fi
  case $p in
    /Library/Keychains/* | /System/*) die "Refusing to touch a system keychain: $p" ;;
  esac
  printf '%s\n' "$p"
}

# identity_available NAME_OR_HASH: is there a code-signing identity (certificate plus private key) like that?
# Trust is deliberately not required (no -v), a self-signed certificate is never "trusted" by macOS.
identity_available() {
  security find-identity -p codesigning ${HK_KEYCHAIN:+"$HK_KEYCHAIN"} 2>/dev/null | grep -F -- "$1" >/dev/null
}

# sign_app APP IDENTITY KIND
#   KIND adhoc : IDENTITY must be "-"; no hardened runtime (nothing to protect, grants reset per build anyway)
#   KIND local : the self-signed hekaton-local identity; hardened runtime, no network timestamp
#   KIND named : a real certificate passed with --identity; hardened runtime + Apple secure timestamp
# No entitlements: hekaton is not App-Sandboxed (Accessibility does not work in a sandbox) and needs no
# hardened-runtime exceptions (no JIT, no third-party dylibs, no Apple Events).
sign_app() {
  local app=$1 ident=$2 kind=$3
  local -a args
  args=(--force --sign "$ident")
  case $kind in
    adhoc) ;;
    local) args+=(--options runtime --timestamp=none) ;;
    named) args+=(--options runtime --timestamp) ;;
    *) die "internal error: unknown signing kind '$kind'" ;;
  esac
  if [ -n "$HK_KEYCHAIN" ]; then
    args+=(--keychain "$HK_KEYCHAIN")
  fi
  codesign "${args[@]}" "$app"
  codesign --verify --strict --deep --verbose=1 "$app"
}

# Version of the hekaton package: cargo if available, else the [workspace.package] version in Cargo.toml.
package_version() {
  local v=""
  if command -v cargo >/dev/null 2>&1; then
    v=$(cd "$REPO_ROOT" && cargo pkgid -p hekaton 2>/dev/null | sed 's/.*[#@]//') || v=""
  fi
  if [ -z "$v" ]; then
    v=$(awk -F'"' '/^\[workspace\.package\]/{f=1;next} /^\[/{f=0} f&&/^version[[:space:]]*=/{print $2; exit}' "$REPO_ROOT/Cargo.toml")
  fi
  printf '%s\n' "$v"
}

# Run `hekaton ARGS...` as the app itself, so macOS attributes permission checks to hekaton.app and not
# to the Terminal that started the script (permission checks follow the responsible process). Output is
# captured through `open --stdout`. Returns 1 if that did not work, so the caller can fall back.
run_as_app() {
  local app=$1 out rc=0
  shift
  out=$(mktemp -t hekaton-run)
  run_with_timeout 45 open -W -n -a "$app" --stdout "$out" --stderr "$out" --args "$@" >/dev/null 2>&1 || rc=$?
  if [ "$rc" -eq 0 ] && [ -s "$out" ]; then
    cat "$out"
    rm -f "$out"
    return 0
  fi
  rm -f "$out"
  return 1
}
