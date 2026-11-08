# Install or update Hekaton on Windows from a GitHub release. No Rust, no build. In PowerShell:
#
#   irm https://github.com/LettuceEatKansas/hekaton/releases/latest/download/install-windows.ps1 | iex
#
# It downloads hekaton-windows-x64.zip, checks it against SHA256SUMS.txt, unpacks it into
# %LOCALAPPDATA%\Programs\Hekaton, puts that folder on your PATH, and registers the MCP server with
# Claude Code when the `claude` command is there. $env:HEKATON_VERSION = 'v0.3.0' picks a release
# (default: the latest). hekaton.exe starts hekatond.exe (the daemon, no console window) from the
# same folder when it is first needed, so there is nothing else to set up.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = 'LettuceEatKansas/hekaton'
$version = if ($env:HEKATON_VERSION) { $env:HEKATON_VERSION } else { 'latest' }
$base = if ($version -eq 'latest') { "https://github.com/$repo/releases/latest/download" }
        else { "https://github.com/$repo/releases/download/$version" }
$dest = Join-Path $env:LOCALAPPDATA 'Programs\Hekaton'
$work = Join-Path ([IO.Path]::GetTempPath()) ("hekaton-install-" + [guid]::NewGuid())
New-Item -ItemType Directory $work | Out-Null

try {
    Write-Host "==> Downloading Hekaton ($version)"
    $zip = Join-Path $work 'hekaton-windows-x64.zip'
    Invoke-WebRequest "$base/hekaton-windows-x64.zip" -OutFile $zip -UseBasicParsing
    $sums = (Invoke-WebRequest "$base/SHA256SUMS.txt" -UseBasicParsing).Content

    Write-Host '==> Checking the download'
    $want = ($sums -split "`n" | Where-Object { $_ -match ' hekaton-windows-x64\.zip\s*$' }) -replace '\s.*$', ''
    $got = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
    if (-not $want -or $want.Trim().ToLower() -ne $got) {
        throw 'The download does not match SHA256SUMS.txt; nothing was installed.'
    }

    $exe = Join-Path $dest 'hekaton.exe'
    if (Test-Path $exe) {
        Write-Host '==> Stopping the running Hekaton daemon for the update'
        & $exe stop 2>&1 | Out-Null
        Start-Sleep -Seconds 1
    }
    New-Item -ItemType Directory -Force $dest | Out-Null
    Expand-Archive -Path $zip -DestinationPath $dest -Force
    Get-ChildItem $dest -Filter *.exe | Unblock-File

    $path = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not (($path -split ';') -contains $dest)) {
        [Environment]::SetEnvironmentVariable('Path', ($path.TrimEnd(';') + ";$dest").TrimStart(';'), 'User')
        Write-Host "==> Added $dest to your PATH (new terminals pick it up)"
    }

    if (Get-Command claude -ErrorAction SilentlyContinue) {
        Write-Host '==> Registering Hekaton with Claude Code'
        & claude mcp remove hekaton -s user 2>&1 | Out-Null
        & claude mcp add -s user hekaton -- $exe mcp
    }

    Write-Host ''
    Write-Host "Hekaton is installed: $exe"
    Write-Host ''
    Write-Host 'Still to do, once:'
    Write-Host "  1. Read and accept the safety notice:   & `"$exe`" acknowledge"
    Write-Host "  2. Any other MCP client: add this server   { `"command`": `"$($exe -replace '\\', '\\')`", `"args`": [`"mcp`"] }"
}
finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
