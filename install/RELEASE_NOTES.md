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
