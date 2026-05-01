# Save as UTF-8 with BOM if Windows PowerShell 5.1 reports parser errors after editing (encoding fallback).
param(
    [Parameter(Position = 0)]
    [ValidateSet("player", "server", "cli", "deps", "help")]
    [string]$Mode = "player",

    [string]$Config = "",
    [switch]$Release,
    [switch]$NoServer,

    # Max seconds to wait for /healthz after spawning the licensing server (default allows first-time cargo build).
    [int]$ServerWaitSeconds = 600,

    # Dev builds only: disable Rust debuginfo/PDB emission (helps avoid MSVC LNK1201 when something locks target\*.pdb).
    [switch]$DevLinkNoPdb,

    # Try to install missing items via winget (Rustup, Git; use -InstallMsvc / -InstallFfmpeg for extras).
    [switch]$InstallDeps,

    # Skip prerequisite checks before player/server/cli (advanced / CI).
    [switch]$SkipDepsCheck,

    # With -InstallDeps: also install VS 2022 Build Tools + VC toolchain (large download).
    [switch]$InstallMsvc,

    # With -InstallDeps: also install FFmpeg (optional; only needed for libsrs_compat ffmpeg / benchmarks).
    [switch]$InstallFfmpeg,

    # Show detailed rustup output when syncing toolchain/components.
    [switch]$VerboseRustup,

    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$CliArgs
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Resolve-Path (Join-Path $ScriptDir "..")

if ([string]::IsNullOrWhiteSpace($Config)) {
    if ($env:SRS_CONFIG_PATH) {
        $ConfigPath = $env:SRS_CONFIG_PATH
    }
    else {
        $ConfigPath = Join-Path $RepoRoot "config\srs.toml"
    }
}
else {
    $ConfigPath = $Config
}

function Show-Usage {
    @"
Usage:
  powershell -ExecutionPolicy Bypass -File tools\run_windows.ps1 [player|server|cli|deps|help] [options] [-- cli args...]

Modes:
  player   Start the local licensing server, then launch the desktop app.
  server   Run only the local licensing server in the foreground.
  cli      Start the server, then run the CLI with the remaining arguments.
  deps     Check (and optionally install) development prerequisites, then exit.
  help     Show this message.

Options:
  -Release        Run Cargo in release mode.
  -NoServer       Skip auto-starting the local licensing server.
  -ServerWaitSeconds N   Wait up to N seconds for the spawned server (default 600). Increase if first compile is slow.
  -DevLinkNoPdb    Dev profile only: set debuginfo=0 so link.exe writes fewer/no PDBs (workaround for LNK1201 / PDB locks).
  -Config PATH    Use a specific config file instead of config\srs.toml.
  -InstallDeps    Install missing requirements via winget where possible (Rustup, Git, FFmpeg;
                  use -InstallMsvc for C++ build tools).
  -InstallMsvc    With -InstallDeps: install VS 2022 Build Tools + VC toolchain (large).
  -SkipDepsCheck  Do not run prerequisite checks before player/server/cli.
  -VerboseRustup  Print full 'rustup show' when syncing toolchain (default: quiet).

Examples:
  powershell -ExecutionPolicy Bypass -File tools\run_windows.ps1
  powershell -ExecutionPolicy Bypass -File tools\run_windows.ps1 -DevLinkNoPdb
  powershell -ExecutionPolicy Bypass -File tools\run_windows.ps1 deps
  powershell -ExecutionPolicy Bypass -File tools\run_windows.ps1 deps -InstallDeps
  powershell -ExecutionPolicy Bypass -File tools\run_windows.ps1 server
  powershell -ExecutionPolicy Bypass -File tools\run_windows.ps1 cli analyze path\to\file.528
"@
}

function Update-SessionPath {
    $machine = [Environment]::GetEnvironmentVariable("Path", "Machine")
    $user = [Environment]::GetEnvironmentVariable("Path", "User")
    $env:Path = "$machine;$user"
}

function Get-WingetPath {
    $cmd = Get-Command "winget.exe" -ErrorAction SilentlyContinue
    if ($cmd) {
        return $cmd.Source
    }
    $wingetApp = Join-Path $env:LOCALAPPDATA "Microsoft\WindowsApps\winget.exe"
    if (Test-Path $wingetApp) {
        return $wingetApp
    }
    return $null
}

function Test-VsWhereVcTools {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) {
        return $false
    }
    $installPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
    return -not [string]::IsNullOrWhiteSpace($installPath)
}

function Get-RustupPath {
    $candidates = @(
        (Join-Path $env:USERPROFILE ".cargo\bin\rustup.exe"),
        (Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe")
    )
    foreach ($p in $candidates) {
        if (Test-Path $p) {
            return (Split-Path $p -Parent)
        }
    }
    return $null
}

function Invoke-WingetInstall {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Id,

        [string]$FriendlyName
    )
    $winget = Get-WingetPath
    if (-not $winget) {
        Write-Warning "winget not found. Install '$FriendlyName' manually (Package Id: $Id)."
        return $false
    }
    Write-Host "Installing $FriendlyName via winget ($Id)..."
    $proc = Start-Process -FilePath $winget -ArgumentList @("install", "-e", "--id", $Id, "--accept-package-agreements", "--accept-source-agreements") -Wait -PassThru
    if ($proc.ExitCode -ne 0 -and $proc.ExitCode -ne $null) {
        # winget sometimes returns non-zero if already installed
        Write-Warning "winget exit code $($proc.ExitCode) for $Id (may already be installed)."
    }
    Update-SessionPath
    return $true
}

function Ensure-RustupToolchain {
    $rustupDir = Get-RustupPath
    $rustup = if ($rustupDir) { Join-Path $rustupDir "rustup.exe" } else { $null }
    if (-not $rustup -or -not (Test-Path $rustup)) {
        return
    }
    Push-Location $RepoRoot
    try {
        Write-Host "Syncing Rust toolchain from rust-toolchain.toml (first run may download stable + components)..."
        if ($VerboseRustup) {
            & $rustup show 2>&1 | Out-Host
        }
        else {
            $null = & $rustup show 2>&1
        }
        foreach ($c in @("rustfmt", "clippy")) {
            $list = @( & $rustup component list --installed 2>$null )
            $has = $list | Where-Object { $_ -match "$c-" } | Select-Object -First 1
            if (-not $has) {
                Write-Host "Adding rustup component: $c"
                & $rustup component add $c 2>&1 | Out-Host
            }
        }
    }
    finally {
        Pop-Location
    }
}

function Get-DepsReport {
    $report = [ordered]@{
        CargoInPath     = [bool](Get-Command cargo -ErrorAction SilentlyContinue)
        RustcInPath     = [bool](Get-Command rustc -ErrorAction SilentlyContinue)
        RustupInPath    = [bool](Get-Command rustup -ErrorAction SilentlyContinue)
        CargoHomeBin    = Test-Path (Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe")
        MsvcVcTools     = Test-VsWhereVcTools
        Git             = [bool](Get-Command git -ErrorAction SilentlyContinue)
        FfmpegOptional  = [bool](Get-Command ffmpeg -ErrorAction SilentlyContinue)
        Winget          = [bool](Get-WingetPath)
    }

    $rustfmtOk = $false
    $clippyOk = $false
    if ($report.RustupInPath) {
        $installed = @( & rustup component list --installed 2>$null )
        if ($installed.Count -gt 0) {
            $rustfmtOk = $null -ne ($installed | Where-Object { $_ -match 'rustfmt-' } | Select-Object -First 1)
            $clippyOk = $null -ne ($installed | Where-Object { $_ -match 'clippy-' } | Select-Object -First 1)
        }
    }
    $report["RustfmtInstalled"] = $rustfmtOk
    $report["ClippyInstalled"] = $clippyOk
    return [pscustomobject]$report
}

function Show-DepsReport {
    param([Parameter(Mandatory = $true)] $Report)
    Write-Host ""
    Write-Host "=== SRS .528 Windows prerequisites ===" -ForegroundColor Cyan
    Write-Host ("{0,-22} {1}" -f "cargo on PATH", $(if ($Report.CargoInPath) { "OK" } else { "MISSING" }))
    Write-Host ("{0,-22} {1}" -f "rustc on PATH", $(if ($Report.RustcInPath) { "OK" } else { "MISSING" }))
    Write-Host ("{0,-22} {1}" -f "rustup on PATH", $(if ($Report.RustupInPath) { "OK" } else { "MISSING" }))
    Write-Host ("{0,-22} {1}" -f "~\.cargo\bin\cargo.exe", $(if ($Report.CargoHomeBin) { "present" } else { "not found" }))
    Write-Host ("{0,-22} {1}" -f "rustfmt (rustup)", $(if ($Report.RustfmtInstalled) { "OK" } else { "missing (workspace wants rustfmt)" }))
    Write-Host ("{0,-22} {1}" -f "clippy (rustup)", $(if ($Report.ClippyInstalled) { "OK" } else { "missing (workspace wants clippy)" }))
    Write-Host ("{0,-22} {1}" -f "MSVC C++ tools", $(if ($Report.MsvcVcTools) { "OK (VS / Build Tools)" } else { "MISSING - cargo link may fail on MSVC host" }))
    Write-Host ("{0,-22} {1}" -f "git", $(if ($Report.Git) { "OK" } else { "optional / recommended" }))
    Write-Host ("{0,-22} {1}" -f "ffmpeg", $(if ($Report.FfmpegOptional) { "OK (optional FFmpeg features)" } else { "optional - not on PATH" }))
    Write-Host ("{0,-22} {1}" -f "winget", $(if ($Report.Winget) { "OK (for -InstallDeps)" } else { "not found" }))
    Write-Host ""
}

function Install-MsvcBuildToolsWinget {
    $winget = Get-WingetPath
    if (-not $winget) {
        Write-Warning "Install 'Visual Studio Build Tools' with C++ workload from https://visualstudio.microsoft.com/visual-cpp-build-tools/"
        return $false
    }
    Write-Host "Installing Visual Studio 2022 Build Tools + VC Tools (this is a large download)..."
    $args = @(
        "install", "-e", "--id", "Microsoft.VisualStudio.2022.BuildTools",
        "--accept-package-agreements", "--accept-source-agreements",
        "--override", "--wait --quiet --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
    )
    $proc = Start-Process -FilePath $winget -ArgumentList $args -Wait -PassThru
    Update-SessionPath
    if (-not (Test-VsWhereVcTools)) {
        Write-Warning "MSVC may still be installing or needs a new terminal. Open 'Developer PowerShell for VS' or restart."
        return $false
    }
    return $true
}

function Ensure-WorkspaceRequirements {
    param(
        [switch]$DoInstall,
        [switch]$DoInstallMsvc
    )

    $report = Get-DepsReport
    if ($Mode -ne "deps") {
        Show-DepsReport -Report $report
    }

    $neededRust = -not $report.CargoInPath -or -not $report.RustcInPath
    $neededMsvc = -not $report.MsvcVcTools

    if ($DoInstall -and $neededRust) {
        if ($report.Winget) {
            Invoke-WingetInstall -Id "Rustlang.Rustup" -FriendlyName "Rustup (Rust toolchain installer)"
            Update-SessionPath
            $report = Get-DepsReport
        }
        else {
            Write-Host "Install Rust from https://rustup.rs/ then re-run this script." -ForegroundColor Yellow
        }
    }

    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        $cargoExe = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
        if (Test-Path $cargoExe) {
            Write-Host "Prepending $([IO.Path]::GetDirectoryName($cargoExe)) to PATH for this session."
            $env:Path = "$(Split-Path $cargoExe -Parent);$env:Path"
            $report = Get-DepsReport
        }
    }

    if (Get-Command rustup -ErrorAction SilentlyContinue) {
        Ensure-RustupToolchain
        $report = Get-DepsReport
    }

    if ($DoInstall -and $neededMsvc -and $DoInstallMsvc) {
        Install-MsvcBuildToolsWinget | Out-Null
        $report = Get-DepsReport
    }

    if ($DoInstall -and (-not $report.Git)) {
        Invoke-WingetInstall -Id "Git.Git" -FriendlyName "Git"
        $report = Get-DepsReport
    }

    if ($DoInstall -and (-not $report.FfmpegOptional)) {
        $ok = Invoke-WingetInstall -Id "Gyan.FFmpeg" -FriendlyName "FFmpeg (Gyan build)"
        if (-not $ok) {
            Invoke-WingetInstall -Id "FFmpeg.FFmpeg" -FriendlyName "FFmpeg" | Out-Null
        }
        Update-SessionPath
        $report = Get-DepsReport
    }

    if ($Mode -eq "deps") {
        $report = Get-DepsReport
        Show-DepsReport -Report $report
    }

    $fail = $false
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Write-Error "cargo is still not available. Install Rust from https://rustup.rs/ or run: tools\run_windows.ps1 deps -InstallDeps"
        $fail = $true
    }
    $report = Get-DepsReport
    if (-not $report.MsvcVcTools) {
        Write-Warning "MSVC C++ build tools not detected. If 'cargo build' fails at link time, install VS Build Tools or run: tools\run_windows.ps1 deps -InstallDeps -InstallMsvc"
    }

    if ($fail) {
        exit 1
    }
}

function Read-BindAddress {
    if (Test-Path $ConfigPath) {
        $match = Select-String -Path $ConfigPath -Pattern '^bind_addr = "([^"]+)"' | Select-Object -First 1
        if ($match) {
            return $match.Matches[0].Groups[1].Value
        }
    }
    return "127.0.0.1:3000"
}

function Ensure-Win32ExitCodeHelper {
    if ($script:SrsWin32ExitCodeLoaded) {
        return
    }
    $src = @'
using System;
using System.Runtime.InteropServices;
namespace Native {
    public static class Win32ExitCode {
        public const uint StillActive = 259;
        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern bool GetExitCodeProcess(IntPtr hProcess, out uint lpExitCode);
    }
}
'@
    try {
        Add-Type -TypeDefinition $src -ErrorAction Stop | Out-Null
    }
    catch {
        # Duplicate load in the same session is fine.
    }
    $script:SrsWin32ExitCodeLoaded = $true
}

function Get-ExitCodeFromStderrText {
    param([string]$Text)
    if ([string]::IsNullOrWhiteSpace($Text)) {
        return $null
    }
    # rustc: linking with `link.exe` failed: exit code: 1201
    $m = [regex]::Match($Text, 'failed:\s*exit code:\s*(\d+)', [System.Text.RegularExpressions.RegexOptions]::IgnoreCase)
    if ($m.Success) {
        return [int]$m.Groups[1].Value
    }
    return $null
}

function Get-ProcessExitCodeDisplay {
    param(
        [System.Diagnostics.Process]$Process,

        # Used when .NET leaves ExitCode unset (common with Start-Process + redirected streams).
        [string]$StderrText = ""
    )

    if (-not $Process) {
        return "n/a"
    }

    try {
        $Process.Refresh()
        if (-not $Process.HasExited) {
            return "(still running)"
        }

        try {
            $null = $Process.WaitForExit()
        }
        catch {
            # ignore
        }

        $code = $Process.ExitCode
        if ($null -ne $code -and "" -ne "$code") {
            return "$code"
        }

        Ensure-Win32ExitCodeHelper
        try {
            $null = $Process.Handle
            [uint32]$native = 0
            if ([Native.Win32ExitCode]::GetExitCodeProcess($Process.Handle, [ref]$native)) {
                if ($native -ne [Native.Win32ExitCode]::StillActive -and $native -le [int32]::MaxValue) {
                    return "$([int]$native)"
                }
            }
        }
        catch {
            # ignore native path failure
        }

        $fromLog = Get-ExitCodeFromStderrText -Text $StderrText
        if ($null -ne $fromLog) {
            return ('(rust/link reported {0} in stderr; cargo.exe exit code not exposed)' -f $fromLog)
        }

        return '(unavailable - cargo/rustc crashed; see stderr log)'
    }
    catch {
        return "(unavailable: $($_.Exception.Message))"
    }
}

function Get-ServerLogTail {
    param(
        [string]$Path,
        [int]$Lines = 40
    )
    if (-not (Test-Path -LiteralPath $Path)) {
        return "(file missing)"
    }
    $tail = @( Get-Content -LiteralPath $Path -Tail $Lines -ErrorAction SilentlyContinue )
    if ($tail.Count -eq 0) {
        return "(empty)"
    }
    return ($tail -join [Environment]::NewLine).TrimEnd()
}

function Wait-ForServer {
    param(
        [System.Diagnostics.Process]$CargoServerProcess,

        [Parameter(Mandatory = $true)]
        [string]$StdoutLogPath,

        [Parameter(Mandatory = $true)]
        [string]$StderrLogPath,

        [int]$TimeoutSec = 600
    )

    $bindAddress = Read-BindAddress
    $parts = $bindAddress.Split(":")
    if ($parts.Length -lt 2) {
        throw "Invalid bind address '$bindAddress'"
    }
    if ($parts[0] -eq "0.0.0.0" -or $parts[0] -eq "*") {
        $serverHost = "127.0.0.1"
    }
    else {
        $serverHost = $parts[0]
    }

    $port = $parts[-1]
    $healthUrl = "http://$serverHost`:$port/healthz"

    $deadline = [datetime]::UtcNow.AddSeconds([Math]::Max(30, $TimeoutSec))
    $sleepMs = 500

    while ([datetime]::UtcNow -lt $deadline) {
        if ($CargoServerProcess) {
            $CargoServerProcess.Refresh()
            if ($CargoServerProcess.HasExited) {
                $stderrTail = Get-ServerLogTail -Path $StderrLogPath
                $stdoutTail = Get-ServerLogTail -Path $StdoutLogPath
                $exitDisp = Get-ProcessExitCodeDisplay -Process $CargoServerProcess -StderrText $stderrTail
                throw @"
Local licensing server process exited before /healthz responded (exit code $exitDisp).
Usually this means 'cargo build' failed for srs_license_server.

Last lines of var\srs_license_server.stderr.log :
---
$stderrTail
---
Last lines of var\srs_license_server.stdout.log :
---
$stdoutTail
---

Hints: linker error LNK1201 on '.pdb' means link.exe could not write the PDB under 'target\'. Common causes: cloud sync (OneDrive on Documents), backup software, another cargo/IDE process, Windows Search indexing, low disk space, or a stale locked file. Try: close other builds; cargo clean; clone/build outside synced folders; -DevLinkNoPdb (fewer PDBs).
Workaround: rerun with -DevLinkNoPdb (disables dev debuginfo / PDB emission). Or: cargo clean then cargo build -p srs_license_server.
"@
            }
        }

        try {
            $response = Invoke-WebRequest -UseBasicParsing -Uri $healthUrl -TimeoutSec 3
            if ($response.StatusCode -eq 200) {
                Write-Host "Licensing server is up ($healthUrl)." -ForegroundColor Green
                return
            }
        }
        catch {
            Start-Sleep -Milliseconds $sleepMs
        }
    }

    $stderrTail = Get-ServerLogTail -Path $StderrLogPath
    $stdoutTail = Get-ServerLogTail -Path $StdoutLogPath
    throw @"
Timed out after ${TimeoutSec}s waiting for licensing server at $healthUrl.

Tail var\srs_license_server.stderr.log :
---
$stderrTail
---
Tail var\srs_license_server.stdout.log :
---
$stdoutTail
---

If cargo was still compiling, increase -ServerWaitSeconds or run once: cargo build -p srs_license_server
If linking failed with LNK1201 on .pdb files, try -DevLinkNoPdb, cargo clean, or build outside cloud-synced folders.
"@
}

function Invoke-CargoPackage {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Package,

        [string[]]$Args = @()
    )

    $cargoArgs = @("run")
    if ($Release) {
        $cargoArgs += "--release"
    }
    $cargoArgs += @("-p", $Package, "--")
    $cargoArgs += $Args

    Push-Location $RepoRoot
    try {
        $env:SRS_CONFIG_PATH = $ConfigPath
        & cargo @cargoArgs
        if ($LASTEXITCODE -ne 0) {
            throw "cargo run failed for package '$Package'"
        }
    }
    finally {
        Pop-Location
    }
}

if ($Mode -eq "help") {
    Show-Usage
    exit 0
}

if ($Mode -eq "deps") {
    Ensure-WorkspaceRequirements -DoInstall:$InstallDeps -DoInstallMsvc:$InstallMsvc
    Write-Host "deps check finished." -ForegroundColor Green
    exit 0
}

if (-not $SkipDepsCheck) {
    Ensure-WorkspaceRequirements -DoInstall:$InstallDeps -DoInstallMsvc:$InstallMsvc
}

if (-not (Test-Path $ConfigPath)) {
    throw "Config file not found: $ConfigPath"
}

if ($DevLinkNoPdb) {
    if ($Release) {
        Write-Host "-DevLinkNoPdb is unnecessary with -Release (release profile avoids full dev PDBs)." -ForegroundColor DarkGray
    }
    else {
        $add = "-C debuginfo=0"
        $rf = $env:RUSTFLAGS
        if ([string]::IsNullOrWhiteSpace($rf)) {
            $env:RUSTFLAGS = $add
        }
        elseif ($rf -notmatch '(?:^|\s)-C\s+debuginfo=') {
            $env:RUSTFLAGS = "$rf $add".Trim()
        }
        $env:CARGO_PROFILE_DEV_DEBUG = "0"
        Write-Host "DevLinkNoPdb: using RUSTFLAGS='$($env:RUSTFLAGS)' (fewer PDB writes; helps LNK1201 / PDB locks)." -ForegroundColor DarkYellow
    }
}

$ServerProcess = $null

try {
    if ($Mode -eq "server") {
        Invoke-CargoPackage -Package "srs_license_server"
        exit 0
    }

    if (-not $NoServer) {
        $VarDir = Join-Path $RepoRoot "var"
        New-Item -ItemType Directory -Force -Path $VarDir | Out-Null
        $stdoutLogPath = Join-Path $VarDir "srs_license_server.stdout.log"
        $stderrLogPath = Join-Path $VarDir "srs_license_server.stderr.log"

        $serverArgs = @("run")
        if ($Release) {
            $serverArgs += "--release"
        }
        $serverArgs += @("-p", "srs_license_server")

        Push-Location $RepoRoot
        try {
            $ServerProcess = Start-Process `
                -FilePath "cargo" `
                -ArgumentList $serverArgs `
                -WorkingDirectory $RepoRoot `
                -RedirectStandardOutput $stdoutLogPath `
                -RedirectStandardError $stderrLogPath `
                -PassThru
        }
        finally {
            Pop-Location
        }

        Wait-ForServer `
            -CargoServerProcess $ServerProcess `
            -StdoutLogPath $stdoutLogPath `
            -StderrLogPath $stderrLogPath `
            -TimeoutSec $ServerWaitSeconds
    }

    switch ($Mode) {
        "player" { Invoke-CargoPackage -Package "srs_player" }
        "cli" { Invoke-CargoPackage -Package "srs_cli" -Args $CliArgs }
        default { throw "Unknown mode '$Mode'" }
    }
}
finally {
    if ($ServerProcess -and -not $ServerProcess.HasExited) {
        Stop-Process -Id $ServerProcess.Id -Force -ErrorAction SilentlyContinue
    }
}
