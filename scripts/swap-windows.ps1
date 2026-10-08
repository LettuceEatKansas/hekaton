<#
.SYNOPSIS
Swap the Windows install of winhands between the Python build and the Rust build, or back.

.DESCRIPTION
  -To rust    stop the running daemon, point the scheduled task "winhands daemon" at winhandsd.exe, set
              daemon_task in config.json, and point the MCP entry `winhands` of Claude Code and of the
              Hermes profiles at `winhands.exe mcp`.
  -To python  the exact reverse (task back to the Python winhands.exe daemon, config.json key removed,
              MCP entries back to python.exe shim.py).

Changing the task needs an elevated PowerShell. Everything else is edited in place, line by line, so
unrelated changes in the Hermes files are kept (the Hermes env blocks, WINHANDS_COORDS included, are
never touched). Each file is copied to <name>.pre-swap-<time>.bak first; the task definition is
exported to XML next to the Rust binaries. -DryRun prints the plan and changes nothing.
See docs/SWAP-WINDOWS.md.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidateSet('rust', 'python')][string]$To,
    [string]$RustBin = 'C:\Users\ikill\tools\winhands-rs-bin',
    [string]$PythonRoot = 'C:\Users\ikill\tools\winhands',
    [string]$TaskName = 'winhands daemon',
    [string[]]$HermesConfigs = @(
        "$env:LOCALAPPDATA\hermes\config.yaml",
        "$env:LOCALAPPDATA\hermes\profiles\lean\config.yaml"),
    [string]$DataDir = "$env:LOCALAPPDATA\winhands",
    [switch]$SkipTask,
    [switch]$SkipClaude,
    [switch]$SkipDaemonStop,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$utf8 = New-Object System.Text.UTF8Encoding($false)

$rustCli = Join-Path $RustBin 'winhands.exe'
$rustDaemon = Join-Path $RustBin 'winhandsd.exe'
$pyExe = Join-Path $PythonRoot 'bin\winhands.exe'
$pyPython = Join-Path $PythonRoot '.venv\Scripts\python.exe'
$pyShim = Join-Path $PythonRoot 'winhands\shim.py'

function Step([string]$what, [scriptblock]$do) {
    if ($DryRun) { Write-Host "[dry-run] $what"; return }
    Write-Host $what
    & $do
}

function Backup([string]$path) {
    Copy-Item -LiteralPath $path -Destination "$path.pre-swap-$stamp.bak"
}

# ---- preflight (nothing is changed before all of this passes)
if ($To -eq 'rust') {
    foreach ($f in @($rustCli, $rustDaemon)) {
        if (-not (Test-Path -LiteralPath $f)) { throw "missing $f (see docs/SWAP-WINDOWS.md, step 1: install the binaries)" }
    }
} else {
    foreach ($f in @($pyExe, $pyPython, $pyShim)) {
        if (-not (Test-Path -LiteralPath $f)) { throw "missing $f (the Python build is not where the rollback expects it)" }
    }
}
if (-not $SkipTask -and -not $DryRun) {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    $admin = ([Security.Principal.WindowsPrincipal]$id).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    if (-not $admin) { throw "changing the task '$TaskName' needs an elevated PowerShell (Run as administrator)" }
    if (-not (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue)) { throw "no scheduled task named '$TaskName'" }
}

# ---- 1. stop whichever daemon runs (the authenticated /shutdown, so an elevated daemon stops too)
if (-not $SkipDaemonStop) {
    $stopper = $pyExe
    if (Test-Path -LiteralPath $rustCli) { $stopper = $rustCli }
    Step "stop the running daemon: $stopper stop" { & $stopper stop | Out-Host }
}

# ---- 2. the scheduled task's action (keeps its principal, run level and settings)
if (-not $SkipTask) {
    if ($To -eq 'rust') {
        $label = $rustDaemon
    } else {
        $label = "$pyExe daemon"
    }
    Step "task '$TaskName' -> $label" {
        if (Test-Path -LiteralPath $RustBin) {
            $xml = Join-Path $RustBin ("task.{0}.pre-swap-{1}.xml" -f ($TaskName -replace '\s', '-'), $stamp)
            schtasks /query /tn $TaskName /xml | Out-File -FilePath $xml -Encoding Unicode
        }
        if ($To -eq 'rust') {
            $action = New-ScheduledTaskAction -Execute $rustDaemon
        } else {
            $action = New-ScheduledTaskAction -Execute $pyExe -Argument 'daemon'
        }
        Set-ScheduledTask -TaskName $TaskName -Action $action | Out-Null
        $now = Get-ScheduledTask -TaskName $TaskName
        Write-Host ("  action now: {0} {1} (run level {2})" -f $now.Actions[0].Execute, $now.Actions[0].Arguments, $now.Principal.RunLevel)
        if ($now.Principal.RunLevel -ne 'Highest') {
            Write-Warning 'run level is no longer Highest: restore the exported XML with schtasks /create /tn "winhands daemon" /xml <file> /f'
        }
    }
}

# ---- 3. config.json: how the Rust shim starts a missing daemon (through the elevated task)
$cfgPath = Join-Path $DataDir 'config.json'
if ($To -eq 'rust') { $cfgLabel = "config.json: daemon_task = '$TaskName'" } else { $cfgLabel = 'config.json: remove daemon_task' }
Step $cfgLabel {
    $obj = $null
    if (Test-Path -LiteralPath $cfgPath) {
        $raw = [IO.File]::ReadAllText($cfgPath, $utf8)
        if ($raw.Trim().Length -gt 0) { $obj = $raw | ConvertFrom-Json }
        Backup $cfgPath
    }
    if ($null -eq $obj) { $obj = New-Object psobject }
    if ($To -eq 'rust') {
        $obj | Add-Member -NotePropertyName daemon_task -NotePropertyValue $TaskName -Force
    } else {
        $obj.PSObject.Properties.Remove('daemon_task')
    }
    if (@($obj.PSObject.Properties).Count -eq 0) {
        Remove-Item -LiteralPath $cfgPath -ErrorAction SilentlyContinue
    } else {
        New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
        [IO.File]::WriteAllText($cfgPath, ($obj | ConvertTo-Json -Depth 10), $utf8)
    }
}

# ---- 4. the Hermes profiles: command and args of mcp_servers.winhands, nothing else
if ($To -eq 'rust') {
    $hCommand = ($rustCli -replace '\\', '/')
    $hArg = 'mcp'
} else {
    $hCommand = ($pyPython -replace '\\', '/')
    $hArg = ($pyShim -replace '\\', '/')
}
$entry = [regex]'(?m)^(  winhands:)(\r?\n)    args:\r?\n(?:      - [^\r\n]*\r?\n)+    command: [^\r\n]*\r?\n'
foreach ($cfg in $HermesConfigs) {
    if (-not (Test-Path -LiteralPath $cfg)) { Write-Host "skip (not there): $cfg"; continue }
    $text = [IO.File]::ReadAllText($cfg, $utf8)
    $from = $text.IndexOf('mcp_servers:')
    if ($from -lt 0) { throw "no mcp_servers: in $cfg" }
    $m = $entry.Match($text, $from)
    if (-not $m.Success) { throw "mcp_servers.winhands in $cfg does not have the expected shape (args list, then command); edit it by hand" }
    $nl = $m.Groups[2].Value
    $new = "  winhands:$nl    args:$nl      - $hArg$nl    command: $hCommand$nl"
    if ($m.Value -eq $new) { Write-Host "already set: $cfg"; continue }
    Step "Hermes entry in $cfg -> $hCommand $hArg" {
        Backup $cfg
        [IO.File]::WriteAllText($cfg, $text.Substring(0, $m.Index) + $new + $text.Substring($m.Index + $m.Length), $utf8)
    }
}

# ---- 5. Claude Code: the user-scope MCP entry (through the CLI, which owns ~/.claude.json)
if (-not $SkipClaude) {
    if (-not (Get-Command claude -ErrorAction SilentlyContinue)) {
        Write-Warning 'claude CLI not on PATH: skipped Claude Code (see docs/SWAP-WINDOWS.md for the command)'
    } else {
        if ($To -eq 'rust') { $cmdLine = @($rustCli, 'mcp') } else { $cmdLine = @($pyPython, $pyShim) }
        Step "Claude Code: claude mcp add -s user winhands -- $($cmdLine -join ' ')" {
            $keep = $ErrorActionPreference
            $ErrorActionPreference = 'Continue'
            & claude mcp remove winhands -s user 2>&1 | Out-Null
            & claude mcp add -s user winhands -- @cmdLine 2>&1 | Out-Host
            $code = $LASTEXITCODE
            $ErrorActionPreference = $keep
            if ($code -ne 0) { throw "claude mcp add failed (exit $code)" }
        }
    }
}

$checker = $pyExe
if ($To -eq 'rust') { $checker = $rustCli }
Write-Host ''
Write-Host "Done: $To. Restart Claude Code and Hermes so they re-read their MCP entries, then check with:"
Write-Host "  & '$checker' start; & '$checker' status"
