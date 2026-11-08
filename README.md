# Hekaton

Hekaton lets an AI agent use a Windows or Mac computer the way a person does. The agent sees the screen, moves the mouse, clicks, types and works through dialogs. It does all of this through one MCP tool, `computer`. Clicks that would be hard to undo (sending, deleting, buying) stop and wait for the computer's owner to approve them.

## Install

The downloads are prebuilt, so you don't need Rust or a build.

**macOS 14 or later.** Run this in Terminal:

```bash
curl -fsSL https://github.com/LettuceEatKansas/hekaton/releases/latest/download/install-mac.sh | bash
```

This puts `hekaton.app` in `~/Applications`. Then, once, open System Settings > Privacy & Security and switch Hekaton on under **Accessibility** and under **Screen & System Audio Recording**.

**Windows 10 or 11.** Run this in PowerShell:

```powershell
irm https://github.com/LettuceEatKansas/hekaton/releases/latest/download/install-windows.ps1 | iex
```

This puts `hekaton.exe` in `%LOCALAPPDATA%\Programs\Hekaton` and adds that folder to your PATH.

Both installers check each download against the release's `SHA256SUMS.txt`. If Claude Code is installed, they register Hekaton with it. To update later, run the same line again. To install a specific version, set `HEKATON_VERSION=v0.3.0` first.

Last step, once on each computer: read and accept the safety notice.

```bash
hekaton acknowledge
```

On macOS, unless you've put the app on your PATH, the full path is `~/Applications/hekaton.app/Contents/MacOS/hekaton`.

Prefer to install by hand? Each [release](https://github.com/LettuceEatKansas/hekaton/releases) has `hekaton-windows-x64.zip` (unzip and run) and `hekaton-macos.zip` (the app plus its `install.sh`).

## Connect an AI agent

Hekaton is an MCP server that speaks over stdio. The command is `hekaton mcp`.

**Claude Code** (the installers already do this):

```bash
claude mcp add -s user hekaton -- /path/to/hekaton mcp
```

**Claude Desktop, Cursor, Hermes and other MCP clients.** Add a server entry like this:

```json
{
  "mcpServers": {
    "hekaton": {
      "command": "/Users/you/Applications/hekaton.app/Contents/MacOS/hekaton",
      "args": ["mcp"]
    }
  }
}
```

On Windows, the command is `C:\\Users\\you\\AppData\\Local\\Programs\\Hekaton\\hekaton.exe`.

**An agent with a shell and no MCP** can use the command line instead: `hekaton hk computer action=screenshot`. Run `hekaton hk help` for the syntax.

**An AI agent installing Hekaton for its user** runs the install line above for that operating system. It then tells the user about the two macOS permission switches and `hekaton acknowledge`, because only a person can do those.

## Safety

- Nothing acts until the owner has run `hekaton acknowledge` once.
- A click that looks hard to undo is refused with a 4-character code. The owner approves it with `hekaton approve CODE` (or denies it with `hekaton deny CODE`) in their own terminal, and the agent then repeats the same call with `approval_code`. Clients that can show an Allow/Deny card ask the owner there instead.
- A pause hotkey stops all input at once, and `hekaton pause` / `hekaton resume` do the same from a terminal.
- Every action is logged to `actions.jsonl` in the data folder: `~/Library/Application Support/hekaton` on macOS, `%LOCALAPPDATA%\hekaton` on Windows.

`hekaton doctor` checks permissions, the display, OCR and the daemon, and says what to fix.

## Build from source

This is only for working on Hekaton itself. You need Rust 1.88 or later. Then:

```bash
cargo build --release -p hekaton
cargo test --workspace
```

On macOS, `packaging/macos/install.sh` builds, signs and installs the app (see `packaging/macos/README.md`). To publish a release, push a `v*` tag; `.github/workflows/release.yml` builds and uploads both downloads.

## License

MIT
