# Block 1: Windows end-to-end SRSV2 benchmark pipeline + H.264-oriented *engineering* progress summary.
# - Writes: var\bench\windows_h264_progress\{summary.json,summary.md,corpus\,reports\}
# - Uses quality_metrics::srsv2_progress_report via bench_srsv2 --h264-progress-summary (strict JSON inputs).
# - FFmpeg is NOT required; optional compare-x264 runs only if `ffmpeg` is on PATH.
# - Does not claim SRSV2 beats H.264 (summary text is measurement-only).

param(
    [int]$Seed = 528,
    [int]$Fps = 30,
    [byte]$Qp = 28
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path -Parent $PSScriptRoot
$OutRoot = Join-Path $RepoRoot "var\bench\windows_h264_progress"
$CorpusDir = Join-Path $OutRoot "corpus"
$ReportsDir = Join-Path $OutRoot "reports"
$CommandLog = Join-Path $OutRoot "commands_run.txt"

if (Test-Path $OutRoot) {
    Remove-Item -Recurse -Force $OutRoot
}
New-Item -ItemType Directory -Force -Path $OutRoot, $CorpusDir, $ReportsDir | Out-Null
Set-Content -Path $CommandLog -Value @(
    "# Windows H.264 progress baseline command log",
    "# repo=$RepoRoot",
    "# seed=$Seed fps=$Fps qp=$Qp"
)

function Join-ArgsForLog {
    param([string[]]$ArgList)
    return ($ArgList | ForEach-Object {
        if ($_ -match '\s') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ }
    }) -join ' '
}

function Invoke-Logged {
    param(
        [string]$Exe,
        [string[]]$ArgList,
        [switch]$AllowFailure
    )
    $line = "$Exe $(Join-ArgsForLog $ArgList)"
    Add-Content -Path $CommandLog -Value $line
    Write-Host ">> $line"
    & $Exe @ArgList
    $code = $LASTEXITCODE
    $script:LastCommandExitCode = $code
    if ($code -ne 0 -and -not $AllowFailure) {
        throw "Command failed with exit code ${code}: $line"
    }
}

Push-Location $RepoRoot
try {
    Invoke-Logged "cargo" @(
        "build",
        "--release",
        "-p", "quality_metrics",
        "--bin", "gen_synthetic_yuv",
        "--bin", "bench_srsv2"
    )

    $TargetDir = Join-Path $RepoRoot "target\release"
    $GenExe = Join-Path $TargetDir "gen_synthetic_yuv.exe"
    $BenchExe = Join-Path $TargetDir "bench_srsv2.exe"
    if (-not (Test-Path $GenExe)) {
        $GenExe = Join-Path $TargetDir "gen_synthetic_yuv"
    }
    if (-not (Test-Path $BenchExe)) {
        $BenchExe = Join-Path $TargetDir "bench_srsv2"
    }

    $Clips = @(
        @{ Tag = "moving_square"; Pattern = "moving-square"; Width = 32; Height = 32; Frames = 6 },
        @{ Tag = "scrolling_bars"; Pattern = "scrolling-bars"; Width = 32; Height = 32; Frames = 6 },
        @{ Tag = "checker"; Pattern = "checker"; Width = 32; Height = 32; Frames = 6 },
        @{ Tag = "scene_cut"; Pattern = "scene-cut"; Width = 32; Height = 32; Frames = 6 }
    )

    foreach ($clip in $Clips) {
        $tag = [string]$clip.Tag
        $pattern = [string]$clip.Pattern
        $width = [int]$clip.Width
        $height = [int]$clip.Height
        $frames = [int]$clip.Frames
        $clipYuv = Join-Path $CorpusDir "$tag.yuv"
        $clipMeta = Join-Path $CorpusDir "$tag.json"
        $clipReports = Join-Path $ReportsDir $tag
        New-Item -ItemType Directory -Force -Path $clipReports | Out-Null

        Invoke-Logged $GenExe @(
            "--pattern", $pattern,
            "--width", "$width",
            "--height", "$height",
            "--frames", "$frames",
            "--fps", "$Fps",
            "--seed", "$Seed",
            "--out", $clipYuv,
            "--meta", $clipMeta
        )

        $common = @(
            "--input", $clipYuv,
            "--width", "$width",
            "--height", "$height",
            "--frames", "$frames",
            "--fps", "$Fps",
            "--qp", "$Qp",
            "--keyint", "6",
            "--motion-radius", "4",
            "--residual-entropy", "auto"
        )

        Invoke-Logged $BenchExe ($common + @(
            "--inter-syntax", "entropy",
            "--compare-entropy-models",
            "--report-json", (Join-Path $clipReports "compare_entropy_models.json"),
            "--report-md", (Join-Path $clipReports "compare_entropy_models.md")
        ))

        Invoke-Logged $BenchExe ($common + @(
            "--inter-syntax", "compact",
            "--compare-partition-costs",
            "--report-json", (Join-Path $clipReports "compare_partition_costs.json"),
            "--report-md", (Join-Path $clipReports "compare_partition_costs.md")
        ))

        Invoke-Logged $BenchExe ($common + @(
            "--sweep-quality-bitrate",
            "--report-json", (Join-Path $clipReports "sweep_quality_bitrate.json"),
            "--report-md", (Join-Path $clipReports "sweep_quality_bitrate.md")
        ))

        Invoke-Logged $BenchExe ($common + @(
            "--compare-b-modes",
            "--reference-frames", "2",
            "--bframes", "1",
            "--report-json", (Join-Path $clipReports "compare_b_modes.json"),
            "--report-md", (Join-Path $clipReports "compare_b_modes.md")
        ))
    }

    $SummaryClip = "moving_square"
    $SummaryReports = Join-Path $ReportsDir $SummaryClip
    $ProgressArgs = @(
        "--h264-progress-summary",
        "--entropy-models-json", (Join-Path $SummaryReports "compare_entropy_models.json"),
        "--partition-costs-json", (Join-Path $SummaryReports "compare_partition_costs.json"),
        "--sweep-quality-bitrate-json", (Join-Path $SummaryReports "sweep_quality_bitrate.json"),
        "--compare-b-modes-json", (Join-Path $SummaryReports "compare_b_modes.json"),
        "--progress-summary-json", (Join-Path $OutRoot "summary.json"),
        "--progress-summary-md", (Join-Path $OutRoot "summary.md")
    )

    $Ffmpeg = Get-Command ffmpeg -ErrorAction SilentlyContinue
    if ($null -ne $Ffmpeg) {
        $x264Json = Join-Path $SummaryReports "compare_x264.json"
        $x264Md = Join-Path $SummaryReports "compare_x264.md"
        $clip = $Clips | Where-Object { $_.Tag -eq $SummaryClip } | Select-Object -First 1
        Invoke-Logged $BenchExe @(
            "--input", (Join-Path $CorpusDir "$SummaryClip.yuv"),
            "--width", "$($clip.Width)",
            "--height", "$($clip.Height)",
            "--frames", "$($clip.Frames)",
            "--fps", "$Fps",
            "--qp", "$Qp",
            "--keyint", "6",
            "--motion-radius", "4",
            "--residual-entropy", "auto",
            "--compare-x264",
            "--report-json", $x264Json,
            "--report-md", $x264Md
        ) -AllowFailure
        if ($script:LastCommandExitCode -eq 0 -and (Test-Path $x264Json)) {
            $ProgressArgs += @("--compare-x264-json", $x264Json)
        } else {
            Add-Content -Path $CommandLog -Value "# optional compare-x264 failed or did not write JSON; continuing without x264"
        }
    } else {
        Add-Content -Path $CommandLog -Value "# ffmpeg not found; optional compare-x264 skipped"
    }

    Invoke-Logged $BenchExe $ProgressArgs

    $SummaryJson = Join-Path $OutRoot "summary.json"
    $SummaryMd = Join-Path $OutRoot "summary.md"
    if (-not (Test-Path $SummaryJson)) {
        throw "Progress summary JSON was not created: $SummaryJson"
    }
    if (-not (Test-Path $SummaryMd)) {
        throw "Progress summary Markdown was not created: $SummaryMd"
    }
    $summary = Get-Content -Raw -Path $SummaryJson | ConvertFrom-Json
    Write-Host "Progress summary written:"
    Write-Host "  JSON: $SummaryJson"
    Write-Host "  MD:   $SummaryMd"
    Write-Host "  next_bottleneck: $($summary.next_bottleneck)"
}
finally {
    Pop-Location
}
