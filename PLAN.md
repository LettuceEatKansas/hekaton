# winhands in Rust: plan

Written 2026-10-08. Owner decision: rewrite winhands in Rust, one codebase for Windows and macOS.
The Python version stays installed and in use until the Rust version passes the parity gate below.

## Why, and what we expect to gain

Measured on the Python build (2026-10-08 audit):

| Cost | Python today | Rust target |
|---|---|---|
| Daemon cold start to healthy | 1.74 s | under 0.2 s |
| First OCR call after start | 1,140 ms | under 250 ms (engine warmed at start) |
| Shim process memory | 57 MB each (12 were running) | under 10 MB |
| Shim startup | 0.41 to 0.48 s | under 50 ms |
| Exe size | 100 MB (PyInstaller, unpacks to temp on every start) | under 15 MB |
| screenshot | 49 ms median | 20 to 35 ms |
| read on a large window | 316 ms (about 290 inside the UIA query) | same floor; UIA cost is Windows' own |

The model's thinking time dominates every desktop task (1,724 s of model time against 31 s of tool time
on the 2026-09-18 Explorer run), so the user-visible gains are mostly cold start, memory, and a single
small signed binary per OS, which also gives macOS one stable identity for its permission grants.

## Ground rules

1. Behavior parity first. The Rust daemon speaks the same HTTP API, the same token and acknowledgement
   rules, and exposes the same MCP `computer` tool (after the 2026-10-08 fixes: `shot`, `shot_long`
   default 1024, `wake`, slimmed schema, settle floor 130 ms) as the Python build. SPEC.md and the
   Python source are the reference. Where they disagree, the Python source wins and SPEC.md gets fixed.
2. Safety layer is ported before any input path ships: acknowledgement gate, destructive refusals
   (destructive.py), caller-window refusal, human-input pause, kill hotkey, `only_apps`, audit log
   with redaction, rate cap, token auth, loopback bind.
3. One `Backend` trait, two implementations. Everything above the trait is platform-agnostic and
   unit-tested on both OSes.
4. No `unsafe` outside the backend modules. Every `unsafe` block carries a one-line reason.
5. The Python build is never removed by this work. The swap is a scheduled-task path change that can
   be reverted in one command.

## Layout

New repository `winhands-rs` (Cargo workspace), separate from the Python repo, whose history carries
361 MB of old executables. Pushed to a private GitHub repository so both Macs can pull it.

```
winhands-rs/
  crates/
    wh-core/      config, token, audit log, ack gate, destructive rules, key-combo parser,
                  batching, expect, settle, coordinate scaling, image encode, the Backend trait
    wh-win/       Windows backend
    wh-mac/       macOS backend
    winhands/     the binary: `winhands daemon | mcp | status | acknowledge | pause | ...`
  tests/parity/   scripts that call the Python and Rust daemons with the same passive requests
  packaging/      Windows scheduled-task setup, macOS .app bundle, LaunchAgent plist, install.sh
```

One binary. `winhands mcp` is the stdio MCP server (what Claude Code and Hermes launch).
`winhands daemon` is the long-lived input and capture service. On Windows the daemon runs elevated
through the existing scheduled task; on macOS it runs as a LaunchAgent inside the signed .app so the
Accessibility and Screen Recording grants attach to one identity.

## Candidate crates (verify current version and maintenance before use)

| Need | Candidate |
|---|---|
| MCP server over stdio | `rmcp` (official modelcontextprotocol/rust-sdk) |
| HTTP daemon | `axum` on `tokio`, or `tiny_http` if a synchronous server is simpler |
| Win32 and WinRT (SendInput, windows, DPI, OCR via Windows.Media.Ocr) | `windows` (microsoft/windows-rs) |
| UI Automation | `uiautomation` (leexgone/uiautomation-rs) or raw `windows` COM |
| Screen capture | `windows` GDI/DXGI directly, or `xcap` (cross-platform) |
| JPEG encode and resize | `image` with `fast_image_resize`, or `jpeg-encoder` |
| macOS input, windows, capture | `objc2` family (`objc2-core-graphics`, `objc2-app-kit`, `objc2-screen-capture-kit`) or `core-graphics` |
| macOS accessibility | `objc2-application-services` or `accessibility-sys` |
| macOS OCR | `objc2-vision` |
| Clipboard | `arboard` |
| Global hotkey | `global-hotkey` |
| Config | `serde` with `toml` |

Reference code that is MIT licensed and may be studied or borrowed with attribution: trycua/cua
(`libs/cua-driver`, Rust, macOS), CursorTouch/MacOS-MCP (Python, AX walking patterns).

## Phases and gates

| Phase | Work | Gate to move on |
|---|---|---|
| R0 | Toolchain, workspace skeleton, crate versions verified, Backend trait drafted from the Python call surface | `cargo build` and `cargo test` green on Windows; GitHub Actions builds on windows-latest and macos-latest |
| R1 | wh-core: config, token, ack gate, audit log, destructive rules (ported with their tests), key parser, batching, expect, settle, scaling, image encode; `winhands mcp` with the exact tool schema | Unit tests ported from the Python suite for these modules pass |
| R2 | wh-win: capture, input, windows and focus, UIA read/snapshot/find, OCR, clipboard, open_app, file_dialog, human-input pause, kill hotkey, caller guard, scheduled-task setup | Parity script: passive actions (screenshot size, list_windows, read, find_text) match Python on the same screen |
| R3 | Active parity: the eval suite (scripts/evals.py core 9 plus held-out 20) against the Rust daemon | Rust scores at least the Python score (22/29 on 2026-09-21); needs the owner away from the PC |
| R4 | Swap on Windows: scheduled task and MCP configs point at the Rust binary; Python kept installed | One week of normal use without a rollback |
| R5 | wh-mac: CGEvent input, ScreenCaptureKit capture, AX tree, Vision OCR, NSWorkspace launch, clipboard, menu-bar pause, signed .app, LaunchAgent, install.sh | Same parity checks on the Mac laptop; owner grants Accessibility and Screen Recording once |

## Known macOS limits to design around

- CGWindowListCreateImage is obsoleted in macOS 15; use ScreenCaptureKit.
- Input coordinates are points, screenshots are pixels; scale by the display's backing factor.
- Grants attach to the signed binary identity. Re-signing with a different identity resets them.
- A daemon outside the user's GUI session cannot see the screen; run as a LaunchAgent in the Aqua session.
- A headless Mac mini may capture a blank or wallpaper-only image. Test the laptop first.
- Password fields under Secure Input, TCC privacy panes and authorization dialogs cannot be driven.
