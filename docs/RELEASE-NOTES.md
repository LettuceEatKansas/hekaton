# Hands development changes

This is an uninstalled development change set. Runtime desktop behavior, macOS permissions, release packaging, and comparative speed remain unverified until their corresponding checks run. Existing installed executables, service registrations, client configurations, and permission identities are unchanged.

## One product, native packages

The new `hands` executable accepts the same commands as `winhands`. For example, `hands mcp` starts the agent interface, `hands status` reads daemon status, and `hands --build-info` prints build provenance as JSON. Existing `winhands`, `winhandsd`, and `wh` entry points remain supported. Windows and macOS still require separate native executables and operating-system adapters.

`--version` now prints the package version followed by source commit, dirty-source status, protocol version, and target. Scripts requiring only a version should read the `version` property of `--build-info`. The authenticated HTTP `/health` response also includes this information under `build`.

Builds read their source identity from Git. A source archive without Git metadata reports `unknown` unless the packager supplies `HANDS_SOURCE_COMMIT` and `HANDS_SOURCE_DIRTY`. A dirty flag means the commit alone does not identify the built source. Release publishers should build clean source, preserve the manifest of artifact hashes, and verify the manifest against the distributed package. These metadata fields identify the claimed source; they do not constitute a signed release attestation.

Changing the product name does not require changing existing state directories, ports, task labels, Mac bundle identifiers, or signing identities. Preserve those identifiers through the compatibility period so saved agent settings and Mac permissions continue to work.

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
