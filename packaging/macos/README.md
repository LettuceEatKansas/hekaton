# macOS packaging

Owner-facing install guide: [docs/INSTALL-mac.md](../../docs/INSTALL-mac.md). This file is for whoever
changes the scripts or the `hekaton` binary.

| File | Job |
| --- | --- |
| `build-app.sh` | Universal (arm64 + x86_64, `lipo`) release binary into `hekaton.app`, then sign. `--binary PATH` skips cargo and packages an existing executable, so it works unchanged for any build of `crates/hekaton`. |
| `make-signing-identity.sh` | One-time: self-signed code-signing certificate `hekaton-local` in the login keychain. Idempotent. |
| `install.sh` | Build or take `--app`, sign, copy to `~/Applications`, write and bootstrap the LaunchAgent, walk through the two permissions, run `doctor`, print the MCP config. |
| `uninstall.sh` | Reverse of install. Leaves TCC entries and the certificate, says how to remove them. |
| `verify-app.sh` | `codesign --verify`, `plutil -lint`, bundle keys, both CPU slices present and runnable. Used by CI. |
| `lib.sh` | Shared helpers (sourced). bash 3.2 compatible, shellcheck clean. |

## Signing and identity

macOS TCC (Accessibility, Screen Recording) stores a code requirement per grant. For an ad hoc signature that
requirement is the code directory hash, which changes with every build, so every update loses the grants. For
a certificate it is `identifier "org.hekaton.app" and certificate leaf = H"<cert hash>"`, which is
stable for as long as the same certificate signs the app. `build-app.sh` picks, in order: `--identity NAME`
(hardened runtime, secure timestamp), the `hekaton-local` identity (hardened runtime, no timestamp), ad hoc
(loud warning). The same bundle id, install path (`~/Applications/hekaton.app`) and certificate must be kept.

No entitlements and no App Sandbox: Accessibility does not work in a sandboxed app, and hekaton needs no
hardened-runtime exceptions (no JIT, no third-party dylibs, no Apple Events). `LSUIElement` is true (no Dock
icon). `LSMinimumSystemVersion` is 14.0 because screenshots use `SCScreenshotManager`.

## LaunchAgent contract

`~/Library/LaunchAgents/org.hekaton.app.plist`, label `org.hekaton.app`:
`ProgramArguments` = `[~/Applications/hekaton.app/Contents/MacOS/hekaton, daemon]`, `RunAtLoad` false,
`KeepAlive` false, `LimitLoadToSessionType` Aqua, `ProcessType` Interactive, `AssociatedBundleIdentifiers` =
the bundle id, stdout and stderr to `~/Library/Logs/hekaton/daemon.log`. `install.sh` bootstraps it into
`gui/<uid>` but does not start it: on demand, like the Windows scheduled task.

Why on demand through launchd and not a child process of `hekaton mcp`: macOS attributes a permission check
to the responsible process. A daemon spawned by `hekaton mcp` would be charged to Claude Code or Terminal and
fail with "not allowed". A daemon started by launchd is its own responsible process, so the grants on
`hekaton.app` apply.

What `crates/hekaton` does to match (`src/start.rs`, `src/cli.rs`, `src/platform.rs`, `hekaton_mac::perm`):

- `hekaton mcp` (and `hk`, `start`), when `127.0.0.1:47811` is not answering and the executable runs from
  inside a `hekaton.app` bundle (or `daemon_launchd_label` / `HEKATON_DAEMON_LAUNCHD_LABEL` is set), runs
  `launchctl kickstart gui/<getuid>/org.hekaton.app` (no `-k`), then polls the port. Only if the
  agent is not loaded (`launchctl print` fails) does it spawn `hekaton daemon` itself, with a warning on
  stderr and in `daemon.log` that permissions then belong to its parent app. `--daemon-cmd` and
  `--daemon-task` still win.
- `hekaton daemon` serves HTTP on a worker thread and calls `hekaton_mac::run_main_run_loop` on the main thread.
- `hekaton doctor` prints the status and exits. `install.sh` runs it as the app with
  `open -W -n -a hekaton.app --stdout FILE --args doctor`, so the result reflects hekaton.app's grants and
  not Terminal's. `doctor --prompt` also triggers the system prompts (`AXIsProcessTrustedWithOptions` with the
  prompt option, `CGRequestPostEventAccess`, `CGRequestScreenCaptureAccess`) so hekaton shows up in the two
  Settings lists without a manual `+`; `install.sh` runs `doctor --prompt` once, before it opens the panes.
- `hekaton --version` (or `--help`) exits 0; CI and `verify-app.sh` run it on both CPU slices.

## Approvals

A step the irreversible-button guard or the destructive hard line would refuse is put to the owner instead, per
step and never sticky (`hekaton_core::approval`). A client that shows MCP elicitation gets an Allow/Deny card; any
other client gets "Needs the owner's OK" with a four-character code. The owner answers a code in their own terminal:

    ~/Applications/hekaton.app/Contents/MacOS/hekaton approve K7QM   # or: deny K7QM

`approve` refuses unless stdin is a terminal, so an agent's non-interactive shell cannot approve its own step,
and the hard line refuses typing the command or an HTTP call to `/approve`. There is no chat hook for codes:
a hook runs without a terminal, so it would pass for an agent too. Deny, no answer or an expired code (10 min;
a card token lasts 5) leaves the refusal as it was.

## CI

`.github/workflows/mac-app.yml`: shellcheck, ad hoc build, `verify-app.sh`, upload `hekaton.app.zip`, then
exercises the owner's flow in a throwaway keychain (identity creation twice, two builds from different
executables with identical designated requirements, install with re-sign, uninstall).
