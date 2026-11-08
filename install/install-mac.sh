#!/bin/bash
# Install or update Hekaton on macOS from a GitHub release. No Rust, no build:
#
#   curl -fsSL https://github.com/LettuceEatKansas/hekaton/releases/latest/download/install-mac.sh | bash
#
# It downloads hekaton-macos.zip, checks it against SHA256SUMS.txt, installs hekaton.app into
# ~/Applications with its LaunchAgent, and registers the MCP server with Claude Code when the `claude`
# command is there. HEKATON_VERSION=v0.3.0 picks a release (default: the latest).
# HEKATON_DRY_RUN=1 stops after the download is checked and unpacked (tests).
set -euo pipefail

REPO="LettuceEatKansas/hekaton"
VERSION="${HEKATON_VERSION:-latest}"
if [ "$VERSION" = latest ]; then
  BASE="https://github.com/$REPO/releases/latest/download"
else
  BASE="https://github.com/$REPO/releases/download/$VERSION"
fi

[ "$(uname -s)" = Darwin ] || { echo "install-mac.sh is for macOS. On Windows use install-windows.ps1." >&2; exit 1; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
echo "==> Downloading Hekaton ($VERSION)"
curl -fsSL -o "$WORK/hekaton-macos.zip" "$BASE/hekaton-macos.zip"
curl -fsSL -o "$WORK/SHA256SUMS.txt" "$BASE/SHA256SUMS.txt"

echo "==> Checking the download"
(cd "$WORK" && grep ' hekaton-macos.zip$' SHA256SUMS.txt | shasum -a 256 -c -) \
  || { echo "The download does not match SHA256SUMS.txt; nothing was installed." >&2; exit 1; }

ditto -x -k "$WORK/hekaton-macos.zip" "$WORK"
if [ "${HEKATON_DRY_RUN:-0}" = 1 ]; then
  ls "$WORK/hekaton-macos"
  codesign -dv "$WORK/hekaton-macos/hekaton.app" 2>&1 | grep -E "^(Identifier|Authority|Signature)=" || true
  echo "dry run: checked and unpacked, nothing installed"
  exit 0
fi
# Keep the signature the release was built with: a Developer ID build keeps its macOS permissions
# across updates, and re-signing here would change the app's identity.
bash "$WORK/hekaton-macos/install.sh" --app "$WORK/hekaton-macos/hekaton.app" --keep-signature --yes

BIN="$HOME/Applications/hekaton.app/Contents/MacOS/hekaton"
if command -v claude >/dev/null 2>&1; then
  echo "==> Registering Hekaton with Claude Code"
  claude mcp remove hekaton -s user >/dev/null 2>&1 || true
  claude mcp add -s user hekaton -- "$BIN" mcp
fi

cat <<EOF

Hekaton is installed: $BIN

Still to do, once:
  1. In System Settings > Privacy & Security, switch Hekaton ON under Accessibility and under
     Screen & System Audio Recording.
  2. Read and accept the safety notice:   "$BIN" acknowledge
  3. Any other MCP client: add this server   { "command": "$BIN", "args": ["mcp"] }
EOF
