param(
    [Parameter(Position = 0)]
    [ValidateSet("player", "server", "cli", "help")]
    [string]$Mode = "player",

    [string]$Config = "",
    [switch]$Release,
    [switch]$NoServer,

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
  powershell -ExecutionPolicy Bypass -File tools\run_windows.ps1 [player|server|cli|help] [options] [-- cli args...]

Modes:
  player   Start the local licensing server, then launch the desktop app.
  server   Run only the local licensing server in the foreground.
  cli      Start the server, then run the CLI with the remaining arguments.
  help     Show this message.

Options:
  -Release        Run Cargo in release mode.
  -NoServer       Skip auto-starting the local licensing server.
  -Config PATH    Use a specific config file instead of config\srs.toml.

Examples:
  powershell -ExecutionPolicy Bypass -File tools\run_windows.ps1
  powershell -ExecutionPolicy Bypass -File tools\run_windows.ps1 server
  powershell -ExecutionPolicy Bypass -File tools\run_windows.ps1 cli analyze path\to\file.528
"@
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

function Wait-ForServer {
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

    for ($attempt = 0; $attempt -lt 50; $attempt++) {
        try {
            $response = Invoke-WebRequest -UseBasicParsing -Uri $healthUrl -TimeoutSec 2
            if ($response.StatusCode -eq 200) {
                return
            }
        }
        catch {
            Start-Sleep -Milliseconds 200
        }
    }

    throw "Local licensing server did not start; see var\srs_license_server.log"
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

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "Missing required command 'cargo'. Install Rust from https://rustup.rs/ or https://www.rust-lang.org/tools/install"
}

if (-not (Test-Path $ConfigPath)) {
    throw "Config file not found: $ConfigPath"
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

        Wait-ForServer
    }

    switch ($Mode) {
        "player" { Invoke-CargoPackage -Package "srs_player" }
        "cli"    { Invoke-CargoPackage -Package "srs_cli" -Args $CliArgs }
        default  { throw "Unknown mode '$Mode'" }
    }
}
finally {
    if ($ServerProcess -and -not $ServerProcess.HasExited) {
        Stop-Process -Id $ServerProcess.Id -Force -ErrorAction SilentlyContinue
    }
}
