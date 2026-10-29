# Hekaton development changes

This is an uninstalled development change set. macOS permissions, release packaging, and the head-to-head comparison with other computer-use tools remain unverified until their corresponding checks run. Existing installed executables, service registrations, client configurations, and permission identities are unchanged.

## One product, native packages

The new `hekaton` executable accepts the same commands as `winhands`. For example, `hekaton mcp` starts the agent interface, `hekaton status` reads daemon status, and `hekaton --build-info` prints build provenance as JSON. Existing `winhands`, `winhandsd`, and `wh` entry points remain supported, and so do the MCP tool name `computer`, the ports, the `%LOCALAPPDATA%\winhands` data directory, the `winhands daemon` scheduled task, the `WINHANDS_*` environment variables and the Mac bundle identifier. An earlier development build called this executable `hands`; that name is gone. Windows and macOS still require separate native executables and operating-system adapters.

## Round 2: capture, settling, reading and office work (2026-10-28 evening)

Measured the same way as the round below: the installed build on port 47811, this build on port 47911, one Windows 11 machine with a 2560 by 1440 primary screen, medians of repeated runs.

| What | Installed | Round 1 | This build |
| --- | --- | --- | --- |
| screenshot, full screen | 66.4 ms | 48.1 ms | 24.9 ms |
| screenshot, one window | 64.0 ms | 49.6 ms | 19.2 ms |
| read, Explorer window | 347.4 ms | 321.9 ms | 93 ms |
| find_text | not measured | 553 ms | 352 ms |
| Notepad gauntlet, one call | refused its own check | 2.7 s | 1.9 s |
| Excel gauntlet, one call | not run | not run | 4.6 s, 5 of 5 |

Screenshots now come from DXGI Desktop Duplication, the compositor's own frame, instead of a GDI copy of the screen. The screenshot the model receives keeps its full size; only the settle loop's private "has the screen stopped moving" checks use small 320 by 180 previews. `WINHANDS_CAPTURE_GDI=1` restores the old capture.

A call that ends with input and returns no picture now finishes its trailing settle in the background. The next call waits for it before doing anything, so no step ever runs against a screen that is still moving. `WINHANDS_SETTLE_SYNC=1` restores the old behavior.

UI Automation reads ask the application for on-screen elements only. Explorer's read went from 325 ms to 93 ms and now lists the visible files instead of navigation-pane items scrolled out of view. `WINHANDS_UIA_OFFSCREEN=1` restores the old query.

Typed text that ends in a newline now presses a real Enter after pasting the rest. Explorer's rename box ignored a pasted newline and kept "New folder".

Under an app allowlist, `open_app` now accepts a bare app name whose `.exe` is on the allowlist, so `open_app "excel"` works with `only_apps: ["excel.exe"]`. A `wait` that is followed by keys or clicks now always waits its full time, because Excel's start screen looks still before it accepts input.

New gauntlet checks, each verified against real state rather than the tool's own message: Calculator (12345 times 6789 read back from the display), Explorer (new folder and F2 rename, checked on disk), window move, minimize, restore, maximize and close (checked through the window list), and Excel (a table with SUM and SUMPRODUCT built through the GUI, saved, and every cell and formula read back from the .xlsx file).

## Safety profiles and the gaming lock

`safety_profile` in `config.json` or `WINHANDS_SAFETY_PROFILE` picks `strict`, `normal` (the default) or `hackathon`. A profile changes limits, never protections: an explicit setting still wins, and the allowlists, the irreversible-button guard, the human-activity pause, the acknowledgement gate, protected processes, `read_only`, stop and cancel all stay on in every profile. `hackathon` shortens the settle and loading waits and raises the clipboard cap. `strict` lengthens them, keeps the human-activity pause on even if it was turned off, refuses acting calls that carry no `only_apps`, and ignores a call's own `allow_destructive` and `allow_irreversible`, because a flag the model wrote is not the owner's approval. A profile name the tool does not know is treated as `strict`. A call cannot choose its profile.

`hekaton lock [reason]` turns on the gaming lock: every input is refused until `hekaton unlock`, while screenshots and reads keep working. Unlocking only works for a person at a real terminal who types `unlock`; run from a non-interactive shell, as an agent would, it refuses and changes nothing. With `game_lock: "auto"` the lock also holds whenever a program from a Steam library (`steamapps\common`) or one named in `game_processes` is running. Status and `/health` report the profile and the lock.

## Excel power channel (Windows)

`{"action":"excel","op":...}` fills and reads spreadsheets through Excel's own automation interface instead of clicking through its window: a 100,000 x 5 block goes in with one call. It is off until the owner sets `power_channels` to `excel` in `config.json` (or `WINHANDS_POWER_CHANNELS=excel`). Ops: `new`, `open` (path), `write` (workbook, sheet, range as the top-left cell, values as rows; a string starting with `=` is a formula), `read` (range like `A1:C5`, `formulas: true` adds the formulas; capped by `excel_read_max_cells`, default 20000, and it says when it cut), `add_sheet`, `calc`, `save_as` (`.xlsx` only, never over an existing file without `overwrite: true`) and `close`. The tool only ever drives a hidden Excel it started itself and refuses an Excel that was already running; macros are disabled on open; closing the last workbook, stopping the daemon or dropping the tool quits that Excel. `read` runs under `read_only` and the gaming lock, everything else does not. An `only_apps` list must include `excel.exe`. Save and open paths get the same system-path refusal as the file dialog, and the strict profile opens only files under `%TEMP%` or ones the call saved. The audit log keeps op, workbook, sheet, range, counts and path, never cell values. Charts and pivot tables are not supported yet.

`--version` now prints the package version followed by source commit, dirty-source status, protocol version, and target. Scripts requiring only a version should read the `version` property of `--build-info`. The authenticated HTTP `/health` response also includes this information under `build`.

Builds read their source identity from Git. A source archive without Git metadata reports `unknown` unless the packager supplies `HANDS_SOURCE_COMMIT` and `HANDS_SOURCE_DIRTY`. A dirty flag means the commit alone does not identify the built source. Release publishers should build clean source, preserve the manifest of artifact hashes, and verify the manifest against the distributed package. These metadata fields identify the claimed source; they do not constitute a signed release attestation.

Changing the product name does not require changing existing state directories, ports, task labels, Mac bundle identifiers, or signing identities. Preserve those identifiers through the compatibility period so saved agent settings and Mac permissions continue to work.

## Measured speed and reliability fixes (2026-10-28)

All numbers below come from live side-by-side runs on the development Windows 11 machine: the installed build on port 47811 and this build on port 47911, driven by `bench/drive.py`, ten repeats per call, median wall time measured by the client. They describe one machine and one screen state, not a general guarantee.

Per-call overhead fell from about 21 ms to under 2 ms. Every request used to rebuild the caller's process ancestry from a full process snapshot. The chain is now cached per process ID and process start time for 60 seconds, so a recycled process ID never inherits another process's chain.

| Call | Installed | This build |
| --- | --- | --- |
| cursor_position | 21.2 ms | 1.8 ms |
| list_windows | 21.5 ms | 1.7 ms |
| screenshot (full screen) | 66.4 ms | 48.1 ms |
| screenshot (one window) | 64.0 ms | 49.6 ms |
| read (Explorer window) | 347.4 ms | 321.9 ms |

`zoom` with text now resolves the exact text of all OCR rows in one UI Automation pass instead of one pass per row. On a region dominated by a text document this took a zoom from 884 ms to about 110 ms with identical output. On regions without document text the time is unchanged.

`profile=true` now returns `metrics.phases`, the wall time and call count spent in capture, encoding, UI Automation, OCR, input, settling and the other phases of the call. Phases are inclusive, so they can sum to more than the call.

Key chords such as ctrl+shift+s now hold the modifiers for 15 ms around the final key. Windows 11 Notepad and other WinUI apps sometimes ignored modifiers sent in a single burst and typed the bare letter.

Under an app allowlist, `wait_for_file` may now watch a file that a `file_dialog` step in the same call has just saved. Previously it refused any path outside the temporary folder, which failed the save-and-verify pattern. Other paths keep the old rule.

Notepad gauntlet (Unicode, emoji, right-to-left and CJK text typed, saved through Save As and verified byte for byte, in one call with no screenshot): this build passed 5 of 5 runs in about 2.7 s and roughly 96 estimated input tokens. The installed build saved correct files 3 of 3 times but its verification step was refused each time.

## Bounded requests and separate controls

The HTTP server uses four desktop workers and an eight-request queue. Health, pause, resume, and shutdown use two separate control workers and their own eight-request queue. A full queue returns HTTP 503 with an explicit statement that the rejected request was not executed.

The bundled HTTP library is patched (`vendor/tiny_http`). It holds at most 32 open connections, gives each one 5 seconds to send its whole request and 10 seconds to receive its response, caps header lines at 8 KiB and headers at 64, rejects ambiguous body framing, and serves one request per connection.

HTTP request bodies are limited to 2 MiB, reduced from the historical 64 MiB limit. Larger calls receive HTTP 413 without execution. Ordinary text and bounded action batches fit within this limit. Callers sending unusually large text or saved recipes must split their requests.

The client adds `X-Hands-Deadline-Unix-Ms`, an absolute Unix timestamp in milliseconds. The server converts it to a monotonic deadline at admission, caps remaining lifetime at ten minutes, and rejects expired queued requests with HTTP 408. Clients without this optional header retain the old request format and receive a two-minute admission budget. Remote clients need reasonably synchronized system clocks when using the absolute header.

Deadlines stop expired queued work and cooperate with core checks before later operations. They cannot undo an action already sent or interrupt an operating-system call that is already blocked. A client timeout still means the outcome may be unknown. Do not automatically retry a timed-out action. Busy errors indicate admission failure; retry only after checking the current state.

This bounds application workers, admitted queues, and per-connection read/write time. It is not comprehensive denial-of-service protection: 32 slow connections can still fill the connection cap for up to 5 seconds each. The control pool isolates control operations from desktop saturation; a separate flood of slow control requests can still occupy that pool.

The persistent agent adapter automatically assigns a stable session identifier to each adapter instance and preserves explicit caller-supplied identifiers. It admits at most four concurrent blocking calls. Session identifiers separate working state; they are not an authentication boundary.

## Verification and historical benchmark correction

`NATIVE-ACCEPTANCE.md` documents the new manually dispatched, self-hosted native workflow, its explicit disposable-desktop opt-in, and its coverage limits. The workflow has not been triggered. Ordinary hosted CI remains insufficient to establish native desktop success.

New socket-level tests use the fake desktop only. They hold four request bodies open, fill the desktop queue, check overflow rejection and independent health/pause access, then verify an expired typing request never reaches the fake input backend. These tests do not operate the user's desktop. Their executed results must be recorded separately after validation.

`legacy-screenspot-timing.patch` is an unapplied patch for the separate Python repository. It changes timing to a monotonic clock and includes OCR time in the per-arm seconds shown by the report. Dataset downloads and shared image preparation remain explicitly excluded. Do not compare corrected numbers with historical reports as if they used the same timing method. ScreenSpot measures screenshot target selection, not complete desktop task reliability or Rust-versus-Python execution speed.
