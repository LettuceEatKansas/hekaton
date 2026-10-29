# Hands development changes

This is an uninstalled development change set. Runtime desktop behavior, macOS permissions, release packaging, and comparative speed remain unverified until their corresponding checks run. Existing installed executables, service registrations, client configurations, and permission identities are unchanged.

## One product, native packages

The new `hands` executable accepts the same commands as `winhands`. For example, `hands mcp` starts the agent interface, `hands status` reads daemon status, and `hands --build-info` prints build provenance as JSON. Existing `winhands`, `winhandsd`, and `wh` entry points remain supported. Windows and macOS still require separate native executables and operating-system adapters.

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
