# hekaton in Rust: plan

Written 2026-10-08. Owner decision: rewrite hekaton in Rust, one codebase for Windows and macOS.
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

New repository `hekaton-rs` (Cargo workspace), separate from the Python repo, whose history carries
361 MB of old executables. Pushed to a private GitHub repository so both Macs can pull it.

```
hekaton-rs/
  crates/
    hekaton-core/      config, token, audit log, ack gate, destructive rules, key-combo parser,
                  batching, expect, settle, coordinate scaling, image encode, the Backend trait
    hekaton-win/       Windows backend
    hekaton-mac/       macOS backend
    hekaton/     the binary: `hekaton daemon | mcp | status | acknowledge | pause | ...`
  tests/parity/   scripts that call the Python and Rust daemons with the same passive requests
  packaging/      Windows scheduled-task setup, macOS .app bundle, LaunchAgent plist, install.sh
```

One binary. `hekaton mcp` is the stdio MCP server (what Claude Code and Hermes launch).
`hekaton daemon` is the long-lived input and capture service. On Windows the daemon runs elevated
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
| R1 | hekaton-core: config, token, ack gate, audit log, destructive rules (ported with their tests), key parser, batching, expect, settle, scaling, image encode; `hekaton mcp` with the exact tool schema | Unit tests ported from the Python suite for these modules pass |
| R1c | Port the 2026-10-08 Python additions: `window` and `folder` shortcuts, window-relative `at`, screenshot `save_to`, URL `open_app`, and the recipe engine (data-only JSON recipes, `when`/`on_fail`, bundled plus user recipes, `recipe_list`/`recipe_save`) | Ported Python tests pass; recipes from the Python build load unchanged |
| R1d | Port the last Python changes of 2026-10-08 (after commit 96026c0) so the Rust build matches the Python one before the Windows swap: a failed `expect` names its step, the agent's own app in front does not pick the monitor, `to_screen` adds the display origin after truncating, `scope` (one window as the display), the slower `drag` (press, 12 moves, 120 ms dwell), the Save As recipe | Ported Python tests (`test_20261008c_display`, `test_20261008d_scope`, `_scope_shim`, `_origin`) pass; the MCP tool definition equals the Python shim's `list_tools`. `scope` crops the screen (a covered window is raised first); reading a covered window without raising it is a later improvement through the OS's own window capture |
| R2 | hekaton-win: capture, input, windows and focus, UIA read/snapshot/find, OCR, clipboard, open_app, file_dialog, human-input pause, kill hotkey, caller guard, scheduled-task setup | Parity script: passive actions (screenshot size, list_windows, read, find_text) match Python on the same screen |
| R3 | Active parity: the eval suite (scripts/evals.py core 9 plus held-out 20) against the Rust daemon | Rust scores at least the Python score (22/29 on 2026-09-21); needs the owner away from the PC |
| R4 | Swap on Windows: scheduled task and MCP configs point at the Rust binary; Python kept installed | One week of normal use without a rollback |
| R5 | hekaton-mac: CGEvent input, ScreenCaptureKit capture, AX tree, Vision OCR, NSWorkspace launch, clipboard, menu-bar pause, signed .app, LaunchAgent, install.sh | Same parity checks on the Mac laptop; owner grants Accessibility and Screen Recording once |

## Known macOS limits to design around

- CGWindowListCreateImage is obsoleted in macOS 15; use ScreenCaptureKit.
- Input coordinates are points, screenshots are pixels; scale by the display's backing factor.
- Grants attach to the signed binary identity. Re-signing with a different identity resets them.
- A daemon outside the user's GUI session cannot see the screen; run as a LaunchAgent in the Aqua session.
- A headless Mac mini may capture a blank or wallpaper-only image. Test the laptop first.
- Password fields under Secure Input, TCC privacy panes and authorization dialogs cannot be driven.

## R2a status: the `hekaton` binary (2026-10-08)

`hekaton daemon | mcp | hk | status | start | stop | pause | resume | acknowledge | doctor`. Run under the
name `hk` (a copy or hard link of the exe) it is the `hk` command line. No argument runs the daemon, as the
Python exe did.

| Piece | Where |
|---|---|
| Request router, auth, ack gate, batch, legacy route adapters, warm-up gate, hotkey chain | `hekaton_core::daemon` (no sockets, tested on every OS) |
| MCP tool definition, payload builder, reply conversion | `hekaton_core::mcp` (compared with `tests/parity/python_list_tools.json`, the Python shim's own `list_tools`) |
| HTTP server (`tiny_http`, one thread per request), idle thread, start-up | `crates/hekaton/src/serve.rs` |
| MCP stdio server (`rmcp` 3.5.1), shim registry and reaper | `shim.rs`, `registry.rs` |
| Client, on-demand daemon start with one retry | `client.rs`, `link.rs`, `start.rs` |
| `hk` syntax | `hk.rs` |

How a missing daemon is started (shim and `hk`), first match wins: `--daemon-cmd "<command line>"` /
`daemon_cmd` in config.json / `HEKATON_DAEMON_CMD`; then `--daemon-task <name>` / `daemon_task` /
`HEKATON_DAEMON_TASK` (`schtasks /run /tn <name>`, how an elevated daemon is started); otherwise this exe
with `daemon`, detached, output to `daemon.log`. `daemon_wait_s` (default 25) bounds the wait for `/health`.

New HTTP routes (the Python daemon has neither): `POST /pause` and `POST /resume`. `/health` gains `impl` and
`backend`. The legacy input routes (`click`, `type`, `key`, `scroll`, `drag`, `hover`, `open`) are adapters
onto `computer` actions; element-number and `t`-ref targets, `window=`, `exact` and the `report` deltas are
refused with a message (see `hekaton_core::daemon::legacy`). `screenshot` returns the whole display as a PNG.

Not done yet: the tray (the pause hotkey and `hekaton pause` replace it), the recipe, `window` and `folder`
actions (R1c; the tool description already names them, the core answers "unknown action" until they land),
signing, and the scheduled-task and LaunchAgent setup.
