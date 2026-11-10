## 0.4.0

Simple mode is new. `hekaton mcp --profile simple` serves nine small text tools (look, read_text, press, type, key, open, scroll, wait, approve) instead of the one big `computer` tool, so small local models can drive the desktop. `--profile simple-vision` adds screenshot and click_at. The default is unchanged. In the full tool, `click` now means `left_click` and `hover` means `mouse_move`.

Fixes in this release:

- Opening a web address no longer reports failure when it worked. A page that opens as a tab in a browser window that already exists now counts as opened when that window's title changed or a window of that browser came to the front.
- Clicking or typing by label is stricter. An exact label beats a longer one that contains it, a button or field beats plain text, a text field wins when you are about to type, and the page beats the browser's own toolbar and bookmarks bar.
- An ambiguous label in simple mode now lists the candidates with the numbers `press` accepts.
- Simple mode's errors and hints name only simple tools and always end with a `Next:` line.
- `look` lists open dialogs, sheets and alerts first, under a "Dialog:" heading.
- `scroll` says when your amount was capped.
- `only_apps` now matches an app by its display name, its process name (with or without .exe) or its bundle id, equality only, and resolves the name given to `open_app` first.

Ready-to-run Hekaton for Windows and macOS. You don't need Rust or a build.

**macOS** (Terminal):

```bash
curl -fsSL https://github.com/LettuceEatKansas/hekaton/releases/latest/download/install-mac.sh | bash
```

**Windows** (PowerShell):

```powershell
irm https://github.com/LettuceEatKansas/hekaton/releases/latest/download/install-windows.ps1 | iex
```

The installers check each download against `SHA256SUMS.txt`, and they register Hekaton with Claude Code if it's installed. Any other MCP client needs the server command `hekaton mcp`. See the README for the full steps.
