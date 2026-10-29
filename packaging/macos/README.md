# macOS packaging

Owner-facing install guide: [docs/INSTALL-mac.md](../../docs/INSTALL-mac.md). This file is for whoever
changes the scripts or the `winhands` binary.

| File | Job |
| --- | --- |
| `build-app.sh` | Universal (arm64 + x86_64, `lipo`) release binary into `winhands.app`, then sign. `--binary PATH` skips cargo and packages an existing executable, so it works unchanged for any build of `crates/winhands`. |
| `make-signing-identity.sh` | One-time: self-signed code-signing certificate `winhands-local` in the login keychain. Idempotent. |
| `install.sh` | Build or take `--app`, sign, copy to `~/Applications`, write and bootstrap the LaunchAgent, walk through the two permissions, run `doctor`, print the MCP config. |
| `uninstall.sh` | Reverse of install. Leaves TCC entries and the certificate, says how to remove them. |
| `verify-app.sh` | `codesign --verify`, `plutil -lint`, bundle keys, both CPU slices present and runnable. Used by CI. |
| `lib.sh` | Shared helpers (sourced). bash 3.2 compatible, shellcheck clean. |

## Signing and identity

macOS TCC (Accessibility, Screen Recording) stores a code requirement per grant. For an ad hoc signature that
requirement is the code directory hash, which changes with every build, so every update loses the grants. For
a certificate it is `identifier "com.lettuceatkansas.winhands" and certificate leaf = H"<cert hash>"`, which is
stable for as long as the same certificate signs the app. `build-app.sh` picks, in order: `--identity NAME`
(hardened runtime, secure timestamp), the `winhands-local` identity (hardened runtime, no timestamp), ad hoc
(loud warning). The same bundle id, install path (`~/Applications/winhands.app`) and certificate must be kept.

No entitlements and no App Sandbox: Accessibility does not work in a sandboxed app, and winhands needs no
hardened-runtime exceptions (no JIT, no third-party dylibs, no Apple Events). `LSUIElement` is true (no Dock
icon). `LSMinimumSystemVersion` is 14.0 because screenshots use `SCScreenshotManager`.

## LaunchAgent contract

`~/Library/LaunchAgents/com.lettuceatkansas.winhands.plist`, label `com.lettuceatkansas.winhands`:
`ProgramArguments` = `[~/Applications/winhands.app/Contents/MacOS/winhands, daemon]`, `RunAtLoad` false,
`KeepAlive` false, `LimitLoadToSessionType` Aqua, `ProcessType` Interactive, `AssociatedBundleIdentifiers` =
the bundle id, stdout and stderr to `~/Library/Logs/winhands/daemon.log`. `install.sh` bootstraps it into
`gui/<uid>` but does not start it: on demand, like the Windows scheduled task.

Why on demand through launchd and not a child process of `winhands mcp`: macOS attributes a permission check
to the responsible process. A daemon spawned by `winhands mcp` would be charged to Claude Code or Terminal and
fail with "not allowed". A daemon started by launchd is its own responsible process, so the grants on
`winhands.app` apply.

What `crates/winhands` does to match (`src/start.rs`, `src/cli.rs`, `src/platform.rs`, `wh_mac::perm`):

- `winhands mcp` (and `wh`, `start`), when `127.0.0.1:47811` is not answering and the executable runs from
  inside a `winhands.app` bundle (or `daemon_launchd_label` / `WINHANDS_DAEMON_LAUNCHD_LABEL` is set), runs
  `launchctl kickstart gui/<getuid>/com.lettuceatkansas.winhands` (no `-k`), then polls the port. Only if the
  agent is not loaded (`launchctl print` fails) does it spawn `winhands daemon` itself, with a warning on
  stderr and in `daemon.log` that permissions then belong to its parent app. `--daemon-cmd` and
  `--daemon-task` still win.
- `winhands daemon` serves HTTP on a worker thread and calls `wh_mac::run_main_run_loop` on the main thread.
- `winhands doctor` prints the status and exits. `install.sh` runs it as the app with
  `open -W -n -a winhands.app --stdout FILE --args doctor`, so the result reflects winhands.app's grants and
  not Terminal's. `doctor --prompt` also triggers the system prompts (`AXIsProcessTrustedWithOptions` with the
  prompt option, `CGRequestPostEventAccess`, `CGRequestScreenCaptureAccess`) so winhands shows up in the two
  Settings lists without a manual `+`; `install.sh` runs `doctor --prompt` once, before it opens the panes.
- `winhands --version` (or `--help`) exits 0; CI and `verify-app.sh` run it on both CPU slices.

## Approvals

A step the irreversible-button guard or the destructive hard line would refuse is put to Jon instead, per
step and never sticky (`wh_core::approval`). A client that shows MCP elicitation gets an Allow/Deny card; any
other client gets "Needs Jon's OK" with a four-character code. Jon answers a code in his own terminal:

    ~/Applications/winhands.app/Contents/MacOS/winhands approve K7QM   # or: deny K7QM

`approve` refuses unless stdin is a terminal, so an agent's non-interactive shell cannot approve its own step,
and the hard line refuses typing the command or an HTTP call to `/approve`. There is no chat hook for codes:
a hook runs without a terminal, so it would pass for an agent too. Deny, no answer or an expired code (10 min;
a card token lasts 5) leaves the refusal as it was.

## CI

`.github/workflows/mac-app.yml`: shellcheck, ad hoc build, `verify-app.sh`, upload `winhands.app.zip`, then
exercises the owner's flow in a throwaway keychain (identity creation twice, two builds from different
executables with identical designated requirements, install with re-sign, uninstall).
