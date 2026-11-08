# Swapping the Windows install between the Python daemon and the Rust one

Written 2026-10-08 14:11 CDT against this machine (task definition, Claude Code entry and both Hermes
files were read that day). The swap has NOT been performed. Everything below is for a quiet moment,
when nothing is using the live daemon (the Python daemon on port 47811 is the live one today).

The pivot is the scheduled task `hekaton daemon`. Every client, Python shim or Rust shim, starts a
missing daemon with `schtasks /run /tn "hekaton daemon"`, so changing that task's action changes
which daemon every client gets. The rest (MCP entries, `config.json`) points the clients at the Rust
shim and lets it use the same elevated task.

## What changes and what does not

| Piece | Python today | Rust after the swap |
|---|---|---|
| Scheduled task `hekaton daemon` (elevated, run level Highest, no trigger, on demand) | `C:\Users\ikill\tools\hekaton\bin\hekaton.exe daemon` | `C:\Users\ikill\tools\hekaton-rs-bin\hekatond.exe` (no console window) |
| Claude Code MCP entry `hekaton` (user scope, `~\.claude.json`) | `...\hekaton\.venv\Scripts\python.exe ...\hekaton\hekaton\shim.py` | `C:\Users\ikill\tools\hekaton-rs-bin\hekaton.exe mcp` |
| Hermes default profile, `%LOCALAPPDATA%\hermes\config.yaml`, `mcp_servers.hekaton` | same python and shim.py | `hekaton.exe` with args `mcp` |
| Hermes lean profile, `%LOCALAPPDATA%\hermes\profiles\lean\config.yaml`, `mcp_servers.hekaton` | same, plus env `HEKATON_PROFILE: lean,computer` and `HEKATON_COORDS: norm1000` | `hekaton.exe mcp`, **env block untouched** (`HEKATON_COORDS: norm1000` stays) |
| `%LOCALAPPDATA%\hekaton\config.json` | absent | `{"daemon_task": "hekaton daemon"}` so the Rust shim starts the daemon through the elevated task |
| Data directory `%LOCALAPPDATA%\hekaton` (token, `acknowledged.json`, `actions.jsonl`, `recipes\`), port 47811, HTTP API, `computer` tool schema | unchanged | unchanged (same files, same port, same tool list byte for byte) |

Things worth knowing:

- The default Hermes profile has no `env` block today (so coordinates are pixels); only the lean profile
  has `HEKATON_COORDS: norm1000`. The swap does not add or remove any env. If the default profile should
  also use `norm1000`, add `env:` with `HEKATON_COORDS: norm1000` under its `hekaton:` entry yourself.
- `HEKATON_PROFILE` is read by neither build (the Python source has no reference to it). It stays as is.
- Python shims that are still running keep working against the Rust daemon (same HTTP API and token).
- `hk` is not on PATH today. To use the Rust one by name, put `C:\Users\ikill\tools\hekaton-rs-bin` on
  the user PATH (`hk.exe` is a copy of `hekaton.exe`; it behaves as `hk` because of its file name).
- The Rust daemon is elevated only because the task runs with the highest run level, exactly like the
  Python one. Started any other way it runs as the plain user.

## Before you start

```powershell
# nothing should be mid-call; this prints the pid and the idle countdown
& C:\Users\ikill\tools\hekaton\bin\hekaton.exe status
Get-ScheduledTask -TaskName 'hekaton daemon' | Select-Object State
```

## Step 1. Install the Rust binaries (no elevation needed)

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
cd C:\Users\ikill\tools\hekaton-rs
cargo build --release -j 4
$bin = 'C:\Users\ikill\tools\hekaton-rs-bin'
New-Item -ItemType Directory -Force $bin | Out-Null
Copy-Item target\release\hekaton.exe, target\release\hekatond.exe $bin -Force
Copy-Item target\release\hekaton.exe "$bin\hk.exe" -Force
```

Optional side-by-side check that touches nothing live (own data dir, test port, no hotkey):

```powershell
$env:LOCALAPPDATA = "$env:TEMP\hk-rs-smoke"; $env:HEKATON_PORT = '47899'
Start-Process "$bin\hekatond.exe" -ArgumentList '--no-hotkey'
Start-Sleep 2; & "$bin\hekaton.exe" status
& "$bin\hekaton.exe" stop
```

Close that PowerShell window afterwards (it has the overridden `LOCALAPPDATA`).

## Step 2. Swap (elevated PowerShell, "Run as administrator")

```powershell
# see the plan first, nothing is changed
powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\ikill\tools\hekaton-rs\scripts\swap-windows.ps1 -To rust -DryRun
# then do it
powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\ikill\tools\hekaton-rs\scripts\swap-windows.ps1 -To rust
```

In order, the script: stops the running daemon (`hekaton.exe stop`, the authenticated shutdown, so the
elevated one stops too); exports the task definition to
`C:\Users\ikill\tools\hekaton-rs-bin\task.hekaton-daemon.pre-swap-<time>.xml`; changes the task action
(`Set-ScheduledTask`, principal, run level and settings are kept) and prints the action and run level
it ended with; sets `daemon_task` in `config.json`; edits the two Hermes files; replaces the Claude Code
entry. Every file it edits is first copied to `<name>.pre-swap-<time>.bak`. It stops at the first error,
and nothing is edited before its checks (binaries exist, running elevated, task exists) pass.

## Step 3. Restart the clients and check

Restart Claude Code and Hermes (both read their MCP entries at start). Then:

```powershell
& C:\Users\ikill\tools\hekaton-rs-bin\hekaton.exe start
& C:\Users\ikill\tools\hekaton-rs-bin\hekaton.exe status
```

`status` should show `implementation: hekaton-rs (windows backend)`, `elevated: True` and
`start method: schtasks /run /tn "hekaton daemon"`. The daemon's own messages are in
`%LOCALAPPDATA%\hekaton\daemon.log`.

## Rollback (one command, elevated PowerShell)

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\ikill\tools\hekaton-rs\scripts\swap-windows.ps1 -To python
```

It stops the Rust daemon, sets the task action back to `C:\Users\ikill\tools\hekaton\bin\hekaton.exe`
with argument `daemon`, removes `daemon_task` from `config.json` (and deletes the file if that leaves it
empty), puts the two Hermes entries and the Claude Code entry back to `python.exe shim.py`, and leaves
the `.bak` copies and the exported task XML where they are. Restart Claude Code and Hermes again. The
Rust files stay installed, so swapping forward again is the same one command.

If the script itself is unusable, restore the task exactly from the exported definition (elevated):

```powershell
schtasks /create /tn "hekaton daemon" /xml "C:\Users\ikill\tools\hekaton-rs-bin\task.hekaton-daemon.pre-swap-<time>.xml" /f
```

## The same steps by hand

Stop the daemon: `& C:\Users\ikill\tools\hekaton-rs-bin\hekaton.exe stop`.

Task action (elevated). Swap to Rust, then back:

```powershell
Set-ScheduledTask -TaskName 'hekaton daemon' -Action (New-ScheduledTaskAction -Execute 'C:\Users\ikill\tools\hekaton-rs-bin\hekatond.exe')
Set-ScheduledTask -TaskName 'hekaton daemon' -Action (New-ScheduledTaskAction -Execute 'C:\Users\ikill\tools\hekaton\bin\hekaton.exe' -Argument 'daemon')
```

Check afterwards: `schtasks /query /tn "hekaton daemon" /xml` must still show `<RunLevel>HighestAvailable</RunLevel>`
and `<LogonType>InteractiveToken</LogonType>`.

Claude Code (`~\.claude.json`, user scope; use the CLI, it owns that file):

```powershell
claude mcp remove hekaton -s user
claude mcp add -s user hekaton -- C:\Users\ikill\tools\hekaton-rs-bin\hekaton.exe mcp
# back to Python:
claude mcp remove hekaton -s user
claude mcp add -s user hekaton -- C:\Users\ikill\tools\hekaton\.venv\Scripts\python.exe C:\Users\ikill\tools\hekaton\hekaton\shim.py
```

Hermes, both files, under `mcp_servers:`. Only `args` and `command` change.

```yaml
# before
  hekaton:
    args:
      - C:/Users/ikill/tools/hekaton/shim.py
    command: C:/Users/ikill/tools/hekaton/.venv/Scripts/python.exe
# after
  hekaton:
    args:
      - mcp
    command: C:/Users/ikill/tools/hekaton-rs-bin/hekaton.exe
```

In the lean file the `enabled: true` and `env:` lines that follow stay exactly as they are:

```yaml
    env:
      HEKATON_PROFILE: lean,computer
      HEKATON_COORDS: norm1000
```

`%LOCALAPPDATA%\hekaton\config.json`:

```json
{"daemon_task": "hekaton daemon"}
```

Without it the Rust shim starts `hekatond.exe` next to itself as the plain user (not elevated, so it
cannot drive elevated windows). `HEKATON_DAEMON_TASK` or `--daemon-task NAME` do the same per process;
`daemon_cmd` / `--daemon-cmd` starts any command instead. Order of choice: command, task, `hekatond.exe`
next to the running exe (Windows), then `hekaton daemon`.

## What was and was not checked

Checked on 2026-10-08: the task definition (action, elevated run level, no trigger) and the three MCP
entries as written above; the script's file edits on copies of the two real Hermes files and in a
throwaway Claude config directory (forward then rollback left both Hermes files byte for byte
identical and the Claude entry in its original shape; a second forward run reports "already set");
`-DryRun`; the Rust exes in a sandbox (data dir and port of their own): the MCP tool list equals the
Python server's, the daemon serves and stops, `hekatond.exe` runs with no console and logs to
`daemon.log`.

Not run, because they need an elevated prompt or touch the live daemon: `Set-ScheduledTask` against the
real task and the daemon stop in the script. The cmdlet syntax was checked (`Get-Command -Syntax`), the
task XML export was checked read-only. If the task's run level or principal looks different after the
change, restore the exported XML with the `schtasks /create ... /f` line above.
