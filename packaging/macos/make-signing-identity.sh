#!/bin/bash
# Create the self-signed code-signing identity "winhands-local" in your LOGIN keychain, once.
#
# Why: macOS remembers the Accessibility and Screen Recording permissions by the app's code signature.
# An ad hoc signature changes with every build, so the permissions would reset on every update. Signing
# every build with this same certificate keeps the signature, and so the permissions, stable.
#
# Safe to run again: if the identity already exists it does nothing. It only ever touches your login
# keychain (never a system keychain) and nothing outside your user account.
set -euo pipefail

# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

usage() {
  cat <<EOF
Usage: make-signing-identity.sh [--yes]

Creates a self-signed code-signing certificate named "$IDENTITY_NAME" in your login keychain.
  --yes   Do not ask for confirmation before creating it.

You are asked for your Mac login password once, so macOS lets the code-signing tool use the new key
without a pop-up on every build. Press Enter to skip that; macOS then shows one "codesign wants to sign
using key" pop-up on the next build, where you click "Always Allow".
EOF
}

while [ $# -gt 0 ]; do
  case $1 in
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
OPENSSL="${OPENSSL:-/usr/bin/openssl}"
[ -x "$OPENSSL" ] || die "openssl not found at $OPENSSL (set OPENSSL=/path/to/openssl)."
need security "It ships with macOS; is this a normal Mac?"
need codesign "It ships with macOS; is this a normal Mac?"

KEYCHAIN=$(login_keychain)
[ -f "$KEYCHAIN" ] || die "Keychain not found: $KEYCHAIN"

if identity_available "$IDENTITY_NAME"; then
  info "The signing identity \"$IDENTITY_NAME\" already exists. Nothing to do."
  exit 0
fi

if security find-certificate -c "$IDENTITY_NAME" "$KEYCHAIN" >/dev/null 2>&1; then
  die "A certificate named \"$IDENTITY_NAME\" exists in $KEYCHAIN but its private key is missing, so it cannot sign.
Remove the broken certificate and run this script again:
    security delete-certificate -c $IDENTITY_NAME \"$KEYCHAIN\""
fi

cat <<EOF

This will create a self-signed code-signing certificate called "$IDENTITY_NAME"
in your LOGIN keychain:
    $KEYCHAIN
It is valid for 10 years and is used only to sign winhands.app on this Mac. It is not trusted
by anyone else and gives winhands no extra powers. Remove it any time with:
    security delete-identity -c $IDENTITY_NAME

EOF
confirm "Create it now?" || die "Cancelled. No identity was created."

WORK=$(mktemp -d -t winhands-identity)
chmod 700 "$WORK"
trap 'rm -rf "$WORK"' EXIT

info "Generating a key and certificate"
cat >"$WORK/openssl.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $IDENTITY_NAME
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
subjectKeyIdentifier = hash
EOF
"$OPENSSL" req -x509 -newkey rsa:2048 -sha256 -nodes -days 3650 \
  -config "$WORK/openssl.cnf" -keyout "$WORK/key.pem" -out "$WORK/cert.pem" 2>/dev/null

# macOS's own LibreSSL (/usr/bin/openssl) writes a .p12 that `security import` accepts. A Homebrew
# OpenSSL 3 would need extra legacy flags, which is why the system one is the default.
P12_PASS=$("$OPENSSL" rand -hex 16)
export WH_P12_PASS="$P12_PASS"
"$OPENSSL" pkcs12 -export -inkey "$WORK/key.pem" -in "$WORK/cert.pem" -name "$IDENTITY_NAME" \
  -out "$WORK/identity.p12" -passout env:WH_P12_PASS

# --- the login password (optional, see usage) ---------------------------------------------------------------
KC_PASS="${WINHANDS_KEYCHAIN_PASSWORD:-}"
if [ -z "$KC_PASS" ] && [ -t 0 ]; then
  printf 'Mac login password (the one that unlocks your keychain; typing is hidden; Enter to skip): '
  read -r -s KC_PASS || true
  printf '\n'
fi

if [ -n "$KC_PASS" ]; then
  security unlock-keychain -p "$KC_PASS" "$KEYCHAIN" >/dev/null 2>&1 || warn "Could not unlock $KEYCHAIN with that password."
fi

info "Importing into $KEYCHAIN"
security import "$WORK/identity.p12" -k "$KEYCHAIN" -P "$P12_PASS" -T /usr/bin/codesign >/dev/null

if [ -n "$KC_PASS" ]; then
  info "Letting the code-signing tool use the key without asking each time"
  # Narrow to this one key first; fall back to all signing keys only if the narrow form did not take.
  security set-key-partition-list -S apple-tool:,apple: -l "$IDENTITY_NAME" -k "$KC_PASS" "$KEYCHAIN" >/dev/null 2>&1 || true

  probe_ok() {
    cp /bin/echo "$WORK/probe"
    run_with_timeout 25 codesign --force --sign "$IDENTITY_NAME" ${WH_KEYCHAIN:+--keychain "$WH_KEYCHAIN"} "$WORK/probe" >/dev/null 2>&1
  }
  if ! probe_ok; then
    note "Narrow permission did not take effect, widening to all signing keys in this keychain."
    security set-key-partition-list -S apple-tool:,apple: -s -k "$KC_PASS" "$KEYCHAIN" >/dev/null 2>&1 || true
    probe_ok || warn "A test signature failed. The next build may show a pop-up: click \"Always Allow\"."
  fi
else
  note "Skipped the login password. The next build will show one pop-up (codesign wants to use key):"
  note "click \"Always Allow\"."
fi

if identity_available "$IDENTITY_NAME"; then
  info "Done. The identity \"$IDENTITY_NAME\" is ready."
  note "Build with packaging/macos/build-app.sh (or run install.sh): it is picked up automatically."
  note "Keep it: if you delete this certificate, the next build gets a different signature and"
  note "macOS asks for the Accessibility and Screen Recording permissions again, once."
else
  die "The certificate was imported but macOS does not list it as a code-signing identity. Check: security find-identity -p codesigning"
fi
