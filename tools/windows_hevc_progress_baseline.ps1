# Windows HEVC-class progress gate for SRSV2 engineering measurements.
# - Writes: var\bench\windows_hevc_progress\{summary.json,summary.md,corpus\,reports\}
# - FFmpeg is optional. x265/x264 rows run only when ffmpeg reports the matching encoder.
# - No H.265 superiority claim: x265 rows are reference measurements, not bitrate-matched proof.

param(
    [int]$Seed = 528,
    [int]$Fps = 30,
    [byte]$Qp = 28
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path -Parent $PSScriptRoot
$OutRoot = Join-Path $RepoRoot "var\bench\windows_hevc_progress"
$CorpusDir = Join-Path $OutRoot "corpus"
$ReportsDir = Join-Path $OutRoot "reports"
$CommandLog = Join-Path $OutRoot "commands_run.txt"
$SummaryJson = Join-Path $OutRoot "summary.json"
$SummaryMd = Join-Path $OutRoot "summary.md"
$DocsMd = Join-Path $RepoRoot "docs\windows_hevc_progress_results.md"

if (Test-Path $OutRoot) {
    Remove-Item -Recurse -Force $OutRoot
}
New-Item -ItemType Directory -Force -Path $OutRoot, $CorpusDir, $ReportsDir | Out-Null
Set-Content -Path $CommandLog -Value @(
    "# Windows HEVC progress gate command log",
    "# repo=$RepoRoot",
    "# seed=$Seed fps=$Fps qp=$Qp",
    "# engineering measurement only; no H.265 superiority claim"
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

function Read-JsonFile {
    param([string]$Path)
    if (-not (Test-Path $Path)) {
        return $null
    }
    return Get-Content -Raw -Path $Path | ConvertFrom-Json
}

function Get-Prop {
    param(
        [object]$Obj,
        [string]$Name
    )
    if ($null -eq $Obj) {
        return $null
    }
    $p = $Obj.PSObject.Properties[$Name]
    if ($null -eq $p) {
        return $null
    }
    return $p.Value
}

function As-Number {
    param([object]$Value, [double]$Default = 0.0)
    if ($null -eq $Value) {
        return $Default
    }
    try {
        return [double]$Value
    } catch {
        return $Default
    }
}

function As-Int64 {
    param([object]$Value, [Int64]$Default = 0)
    if ($null -eq $Value) {
        return $Default
    }
    try {
        return [Int64]$Value
    } catch {
        return $Default
    }
}

function New-Candidate {
    param(
        [string]$Clip,
        [string]$Label,
        [string]$Mode,
        [object]$Source,
        [string]$Kind
    )
    $rowObj = Get-Prop $Source "row"
    if ($null -ne $rowObj -and $rowObj.GetType().Name -ne "String") {
        $bytes = As-Int64 (Get-Prop $rowObj "bytes")
        $psnr = As-Number (Get-Prop $rowObj "psnr_y")
        $ssim = As-Number (Get-Prop $rowObj "ssim_y")
        $bitrate = As-Number (Get-Prop $rowObj "bitrate_bps")
    } else {
        $bytes = As-Int64 (Get-Prop $Source "total_bytes")
        if ($bytes -eq 0) {
            $bytes = As-Int64 (Get-Prop $Source "bytes")
        }
        $psnr = As-Number (Get-Prop $Source "psnr_y")
        $ssim = As-Number (Get-Prop $Source "ssim_y")
        $bitrate = As-Number (Get-Prop $Source "bitrate_bps")
    }

    $okProp = Get-Prop $Source "ok"
    $ok = $true
    if ($null -ne $okProp) {
        $ok = [bool]$okProp
    }

    return [PSCustomObject]@{
        clip = $Clip
        label = $Label
        mode = $Mode
        kind = $Kind
        ok = $ok
        bytes = $bytes
        psnr_y = $psnr
        ssim_y = $ssim
        bitrate_bps = $bitrate
        source = $Source
    }
}

function Select-BestBytes {
    param([object[]]$Rows)
    $valid = @($Rows | Where-Object { $_.ok -and $_.bytes -gt 0 })
    if ($valid.Count -eq 0) {
        return $null
    }
    return $valid | Sort-Object @{ Expression = "bytes"; Ascending = $true }, @{ Expression = "ssim_y"; Ascending = $false }, @{ Expression = "psnr_y"; Ascending = $false } | Select-Object -First 1
}

function Select-BestQuality {
    param([object[]]$Rows)
    $valid = @($Rows | Where-Object { $_.ok -and $_.bytes -gt 0 })
    if ($valid.Count -eq 0) {
        return $null
    }
    return $valid | Sort-Object @{ Expression = "ssim_y"; Ascending = $false }, @{ Expression = "psnr_y"; Ascending = $false }, @{ Expression = "bytes"; Ascending = $true } | Select-Object -First 1
}

function Format-CandidateLine {
    param([object]$Row)
    if ($null -eq $Row) {
        return "_unavailable_"
    }
    return "clip=$($Row.clip), label=$($Row.label), mode=$($Row.mode), bytes=$($Row.bytes), PSNR-Y=$([Math]::Round($Row.psnr_y, 4)), SSIM-Y=$([Math]::Round($Row.ssim_y, 4))"
}

function Format-YesNo {
    param([bool]$Value)
    if ($Value) { return "yes" }
    return "no"
}

function Get-CodecRow {
    param(
        [object]$Report,
        [string]$Codec
    )
    foreach ($row in @((Get-Prop $Report "table"))) {
        if ((Get-Prop $row "codec") -eq $Codec) {
            return $row
        }
    }
    return $null
}

function Detect-FfmpegEncoders {
    $ffmpeg = Get-Command ffmpeg -ErrorAction SilentlyContinue
    if ($null -eq $ffmpeg) {
        return [PSCustomObject]@{ ffmpeg = $false; libx264 = $false; libx265 = $false; text = "" }
    }
    $encText = (& ffmpeg -hide_banner -encoders 2>&1 | Out-String)
    $tokens = @($encText -split '\s+')
    return [PSCustomObject]@{
        ffmpeg = $true
        libx264 = ($tokens -contains "libx264")
        libx265 = ($tokens -contains "libx265")
        text = $encText
    }
}

function Select-NextFeature {
    param(
        [object]$Bottleneck,
        [object]$X265Result,
        [object]$BestPartitionV1,
        [object]$BestPartitionV2,
        [object]$BModes
    )

    if ($null -ne $BestPartitionV1 -and $null -ne $BestPartitionV2 -and $BestPartitionV2.bytes -lt $BestPartitionV1.bytes) {
        return [PSCustomObject]@{
            id = "B"
            name = "bounded quadtree partitions"
            why = "Partition syntax v2 saved bytes in the gate; the next real partition step is bounded quadtree decisions rather than changing codec claims."
        }
    }

    switch ($Bottleneck.name) {
        "residual" {
            return [PSCustomObject]@{ id = "C"; name = "context-adaptive residual coefficient entropy"; why = "Inter residual bytes are the largest named bucket." }
        }
        "partition syntax" {
            return [PSCustomObject]@{ id = "B"; name = "bounded quadtree partitions"; why = "Partition map bytes are the largest named bucket." }
        }
        "MV/header" {
            return [PSCustomObject]@{ id = "D"; name = "quarter-pel luma motion"; why = "Motion/header bytes dominate among named buckets." }
        }
        "transform syntax" {
            return [PSCustomObject]@{ id = "B"; name = "bounded quadtree partitions"; why = "Transform/partition syntax bytes dominate among named buckets." }
        }
        "intra/keyframe cost" {
            return [PSCustomObject]@{ id = "A"; name = "CTU-style 64x64 superblocks"; why = "Intra/keyframe cost is the largest named bucket, so larger superblock structure is the next broad compression lever." }
        }
        default {
            if ($null -ne $BModes -and $BModes.b_half_beats_b_int_count -gt 0) {
                return [PSCustomObject]@{ id = "D"; name = "quarter-pel luma motion"; why = "At least one clip saw B-half reduce bytes vs B-int; motion precision deserves the next focused gate." }
            }
            if ($null -ne $X265Result -and $X265Result.status -eq "ok" -and $X265Result.srsv2_bitrate_bps -gt 0 -and $X265Result.x265_bitrate_bps -gt 0) {
                $ratio = [Math]::Abs($X265Result.srsv2_bitrate_bps - $X265Result.x265_bitrate_bps) / [Math]::Max($X265Result.srsv2_bitrate_bps, $X265Result.x265_bitrate_bps)
                if ($ratio -gt 0.10) {
                    return [PSCustomObject]@{
                        id = "G"
                        name = "bitrate-matched x265 sweep"
                        why = "Optional x265 ran, but achieved bitrate differs materially from SRSV2 (relative gap $([Math]::Round($ratio, 4))). The next fair gate is bitrate matching, not a superiority claim."
                    }
                }
            }
            return [PSCustomObject]@{ id = "A"; name = "CTU-style 64x64 superblocks"; why = "The largest bucket is unresolved/other payload after current map and MV buckets; larger superblock structure is the next broad compression lever." }
        }
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
        @{ Tag = "moving_square"; Pattern = "moving-square"; Width = 64; Height = 64; Frames = 8 },
        @{ Tag = "scrolling_bars"; Pattern = "scrolling-bars"; Width = 64; Height = 64; Frames = 8 },
        @{ Tag = "checker"; Pattern = "checker"; Width = 64; Height = 64; Frames = 8 },
        @{ Tag = "scene_cut"; Pattern = "scene-cut"; Width = 64; Height = 64; Frames = 8 }
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
            "--keyint", "$frames",
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
            "--inter-syntax", "compact",
            "--compare-partition-syntax",
            "--report-json", (Join-Path $clipReports "compare_partition_syntax.json"),
            "--report-md", (Join-Path $clipReports "compare_partition_syntax.md")
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

    $encoders = Detect-FfmpegEncoders
    Add-Content -Path $CommandLog -Value "# ffmpeg=$($encoders.ffmpeg) libx264=$($encoders.libx264) libx265=$($encoders.libx265)"
    $SummaryClip = "moving_square"
    $summaryClipSpec = $Clips | Where-Object { $_.Tag -eq $SummaryClip } | Select-Object -First 1
    $summaryClipReports = Join-Path $ReportsDir $SummaryClip
    $summaryClipYuv = Join-Path $CorpusDir "$SummaryClip.yuv"

    if ($encoders.libx265) {
        Invoke-Logged $BenchExe @(
            "--input", $summaryClipYuv,
            "--width", "$($summaryClipSpec.Width)",
            "--height", "$($summaryClipSpec.Height)",
            "--frames", "$($summaryClipSpec.Frames)",
            "--fps", "$Fps",
            "--qp", "$Qp",
            "--keyint", "$($summaryClipSpec.Frames)",
            "--motion-radius", "4",
            "--residual-entropy", "auto",
            "--compare-x265",
            "--x265-crf", "28",
            "--x265-preset", "medium",
            "--report-json", (Join-Path $summaryClipReports "compare_x265.json"),
            "--report-md", (Join-Path $summaryClipReports "compare_x265.md")
        ) -AllowFailure
    } else {
        Add-Content -Path $CommandLog -Value "# ffmpeg/libx265 unavailable; optional compare-x265 skipped"
    }

    if ($encoders.libx264 -and $encoders.libx265) {
        Invoke-Logged $BenchExe @(
            "--input", $summaryClipYuv,
            "--width", "$($summaryClipSpec.Width)",
            "--height", "$($summaryClipSpec.Height)",
            "--frames", "$($summaryClipSpec.Frames)",
            "--fps", "$Fps",
            "--qp", "$Qp",
            "--keyint", "$($summaryClipSpec.Frames)",
            "--motion-radius", "4",
            "--residual-entropy", "auto",
            "--compare-x264-and-x265",
            "--x264-crf", "23",
            "--x264-preset", "medium",
            "--x265-crf", "28",
            "--x265-preset", "medium",
            "--report-json", (Join-Path $summaryClipReports "compare_x264_and_x265.json"),
            "--report-md", (Join-Path $summaryClipReports "compare_x264_and_x265.md")
        ) -AllowFailure
    } else {
        Add-Content -Path $CommandLog -Value "# libx264+libx265 pair unavailable; optional compare-x264-and-x265 skipped"
    }

    $staticRows = @()
    $contextRows = @()
    $partSyntaxV1Rows = @()
    $partSyntaxV2Rows = @()
    $partSyntaxPairs = @()
    $sweepRows = @()
    $bModeRows = @()
    $costRows = @()

    foreach ($clip in $Clips) {
        $tag = [string]$clip.Tag
        $clipReports = Join-Path $ReportsDir $tag

        $entropy = Read-JsonFile (Join-Path $clipReports "compare_entropy_models.json")
        foreach ($entry in @((Get-Prop $entropy "compare_entropy_models"))) {
            $mode = ([string](Get-Prop $entry "entropy_model_mode")).ToLowerInvariant()
            if ($mode -eq "static" -or $mode -eq "staticv1") {
                $staticRows += New-Candidate $tag "SRSV2-entropy-StaticV1" $mode $entry "entropy"
            } elseif ($mode -eq "context" -or $mode -eq "contextv1") {
                $contextRows += New-Candidate $tag "SRSV2-entropy-ContextV1" $mode $entry "entropy"
            }
        }

        $partSyntax = Read-JsonFile (Join-Path $clipReports "compare_partition_syntax.json")
        $partSyntaxEntries = @((Get-Prop $partSyntax "compare_partition_syntax"))
        foreach ($entry in @((Get-Prop $partSyntax "compare_partition_syntax"))) {
            $mode = [string](Get-Prop $entry "partition_syntax_mode")
            $rowId = [string](Get-Prop $entry "row")
            if ($mode -eq "v1") {
                $partSyntaxV1Rows += New-Candidate $tag $rowId $mode $entry "partition_syntax"
            } elseif ($mode -eq "v2") {
                $partSyntaxV2Rows += New-Candidate $tag $rowId $mode $entry "partition_syntax"
            }
        }
        foreach ($baseName in @("auto-fast-rdo", "split8x8")) {
            $v1 = $partSyntaxEntries | Where-Object { (Get-Prop $_ "row") -eq "$baseName-v1" } | Select-Object -First 1
            $v2 = $partSyntaxEntries | Where-Object { (Get-Prop $_ "row") -eq "$baseName-v2" } | Select-Object -First 1
            if ($null -ne $v1 -and $null -ne $v2) {
                $v1Total = As-Int64 (Get-Prop $v1 "total_bytes")
                $v2Total = As-Int64 (Get-Prop $v2 "total_bytes")
                $v1Map = As-Int64 (Get-Prop $v1 "partition_map_v1_bytes")
                $v2Map = As-Int64 (Get-Prop $v2 "partition_map_v2_bytes")
                $partSyntaxPairs += [PSCustomObject]@{
                    clip = $tag
                    pair = $baseName
                    v1_total_bytes = $v1Total
                    v2_total_bytes = $v2Total
                    total_delta_v2_minus_v1 = $v2Total - $v1Total
                    v1_map_bytes = $v1Map
                    v2_map_bytes = $v2Map
                    map_delta_v2_minus_v1 = $v2Map - $v1Map
                    v2_smaller_total = ($v2Total -gt 0 -and $v2Total -lt $v1Total)
                    v2_smaller_map = ($v2Map -lt $v1Map)
                    v1_ok = [bool](Get-Prop $v1 "ok")
                    v2_ok = [bool](Get-Prop $v2 "ok")
                }
            }
        }

        $sweep = Read-JsonFile (Join-Path $clipReports "sweep_quality_bitrate.json")
        foreach ($entry in @((Get-Prop $sweep "rows"))) {
            $label = "row_$((Get-Prop $entry "row_index"))"
            $mode = "$((Get-Prop $entry "inter_syntax"))/$((Get-Prop $entry "entropy_model"))/$((Get-Prop $entry "partition_cost_model"))/$((Get-Prop $entry "inter_partition"))"
            $sweepRows += New-Candidate $tag $label $mode $entry "sweep"
        }

        $b = Read-JsonFile (Join-Path $clipReports "compare_b_modes.json")
        foreach ($entry in @((Get-Prop $b "compare_b_modes"))) {
            $row = Get-Prop $entry "row"
            $bModeRows += [PSCustomObject]@{
                clip = $tag
                mode = [string](Get-Prop $entry "mode")
                bytes = As-Int64 (Get-Prop $row "bytes")
                psnr_y = As-Number (Get-Prop $row "psnr_y")
                ssim_y = As-Number (Get-Prop $row "ssim_y")
            }
        }

        $cost = Read-JsonFile (Join-Path $clipReports "compare_partition_costs.json")
        foreach ($entry in @((Get-Prop $cost "compare_partition_costs"))) {
            $cand = New-Candidate $tag ([string](Get-Prop $entry "label")) "" $entry "partition_costs"
            $cand | Add-Member -NotePropertyName details -NotePropertyValue (Get-Prop $entry "details")
            $costRows += $cand
        }
    }

    $bestStatic = Select-BestBytes $staticRows
    $bestContext = Select-BestBytes $contextRows
    $bestPartSyntaxV1 = Select-BestBytes $partSyntaxV1Rows
    $bestPartSyntaxV2 = Select-BestBytes $partSyntaxV2Rows
    $bestSweep = Select-BestQuality $sweepRows

    $contextPairs = @()
    foreach ($clip in $Clips) {
        $tag = [string]$clip.Tag
        $s = $staticRows | Where-Object { $_.clip -eq $tag } | Select-Object -First 1
        $c = $contextRows | Where-Object { $_.clip -eq $tag } | Select-Object -First 1
        if ($null -ne $s -and $null -ne $c) {
            $contextPairs += [PSCustomObject]@{
                clip = $tag
                static_bytes = $s.bytes
                context_bytes = $c.bytes
                delta_context_minus_static = $c.bytes - $s.bytes
                context_smaller = ($c.bytes -lt $s.bytes)
            }
        }
    }

    $autoFastPairs = @()
    foreach ($clip in $Clips) {
        $tag = [string]$clip.Tag
        $fixed = $costRows | Where-Object { $_.clip -eq $tag -and $_.label -eq "SRSV2-pc-fixed16x16" } | Select-Object -First 1
        $auto = $costRows | Where-Object { $_.clip -eq $tag -and $_.label -eq "SRSV2-pc-auto-fast-rdo" } | Select-Object -First 1
        if ($null -ne $fixed -and $null -ne $auto) {
            $autoFastPairs += [PSCustomObject]@{
                clip = $tag
                fixed16x16_bytes = $fixed.bytes
                auto_fast_rdo_bytes = $auto.bytes
                delta_auto_minus_fixed = $auto.bytes - $fixed.bytes
                auto_fast_rdo_smaller = ($auto.bytes -lt $fixed.bytes)
            }
        }
    }

    $bSummary = @()
    $bHalfBeats = 0
    $bWeightedBeats = 0
    foreach ($clip in $Clips) {
        $tag = [string]$clip.Tag
        $rows = @($bModeRows | Where-Object { $_.clip -eq $tag })
        $pOnly = $rows | Where-Object { $_.mode -eq "SRSV2-P-only" } | Select-Object -First 1
        $bInt = $rows | Where-Object { $_.mode -eq "SRSV2-B-int" } | Select-Object -First 1
        $bHalf = $rows | Where-Object { $_.mode -eq "SRSV2-B-half" } | Select-Object -First 1
        $bWeighted = $rows | Where-Object { $_.mode -eq "SRSV2-B-weighted" } | Select-Object -First 1
        if ($null -ne $bInt -and $null -ne $bHalf -and $bHalf.bytes -lt $bInt.bytes) { $bHalfBeats += 1 }
        if ($null -ne $bInt -and $null -ne $bWeighted -and $bWeighted.bytes -lt $bInt.bytes) { $bWeightedBeats += 1 }
        $bSummary += [PSCustomObject]@{
            clip = $tag
            p_only_bytes = if ($null -ne $pOnly) { $pOnly.bytes } else { $null }
            b_int_bytes = if ($null -ne $bInt) { $bInt.bytes } else { $null }
            b_half_bytes = if ($null -ne $bHalf) { $bHalf.bytes } else { $null }
            b_weighted_bytes = if ($null -ne $bWeighted) { $bWeighted.bytes } else { $null }
        }
    }
    $bModeAggregate = [PSCustomObject]@{
        b_half_beats_b_int_count = $bHalfBeats
        b_weighted_beats_b_int_count = $bWeightedBeats
        rows = $bSummary
    }

    $x265Report = Read-JsonFile (Join-Path $summaryClipReports "compare_x265.json")
    if ($null -eq $x265Report) {
        $x265Report = Read-JsonFile (Join-Path $summaryClipReports "compare_x264_and_x265.json")
    }
    $x265Status = "skipped: ffmpeg/libx265 unavailable"
    $x265Result = $null
    if ($null -ne $x265Report) {
        $x265Obj = Get-Prop $x265Report "x265"
        $srsv2Row = Get-CodecRow $x265Report "SRSV2"
        if ($null -eq $srsv2Row) {
            $srsv2Row = Get-CodecRow $x265Report "SRSV2-ps-fixed16x16"
        }
        if ($null -ne $x265Obj) {
            $x265Status = [string](Get-Prop $x265Obj "x265_status")
            $x265Result = [PSCustomObject]@{
                status = $x265Status
                command = Get-Prop $x265Obj "x265_command"
                bytes = As-Int64 (Get-Prop $x265Obj "x265_bytes")
                x265_bitrate_bps = As-Number (Get-Prop $x265Obj "x265_bitrate_bps")
                psnr_y = As-Number (Get-Prop $x265Obj "x265_psnr_y")
                ssim_y = As-Number (Get-Prop $x265Obj "x265_ssim_y")
                encode_seconds = As-Number (Get-Prop $x265Obj "x265_encode_seconds")
                decode_seconds = As-Number (Get-Prop $x265Obj "x265_decode_seconds")
                srsv2_bytes = if ($null -ne $srsv2Row) { As-Int64 (Get-Prop $srsv2Row "bytes") } else { 0 }
                srsv2_bitrate_bps = if ($null -ne $srsv2Row) { As-Number (Get-Prop $srsv2Row "bitrate_bps") } else { 0.0 }
            }
        }
    }

    $x265Approach = [PSCustomObject]@{
        answered = ($null -ne $x265Result)
        similar_bitrate = $false
        bitrate_relative_gap = $null
        srsv2_bitrate_bps = if ($null -ne $x265Result) { $x265Result.srsv2_bitrate_bps } else { 0.0 }
        x265_bitrate_bps = if ($null -ne $x265Result) { $x265Result.x265_bitrate_bps } else { 0.0 }
        summary = "x265 unavailable; skipped cleanly."
    }
    if ($null -ne $x265Result) {
        $gap = 0.0
        if ($x265Result.srsv2_bitrate_bps -gt 0 -and $x265Result.x265_bitrate_bps -gt 0) {
            $gap = [Math]::Abs($x265Result.srsv2_bitrate_bps - $x265Result.x265_bitrate_bps) / [Math]::Max($x265Result.srsv2_bitrate_bps, $x265Result.x265_bitrate_bps)
        }
        $x265Approach.bitrate_relative_gap = $gap
        $x265Approach.similar_bitrate = ($gap -le 0.10)
        if ($x265Result.status -ne "ok") {
            $x265Approach.summary = "x265 report status is '$($x265Result.status)'; comparison is unavailable."
        } elseif ($x265Approach.similar_bitrate) {
            $x265Approach.summary = "SRSV2 and x265 are within 10% achieved bitrate on the optional row; inspect quality metrics, but this is still not superiority proof."
        } else {
            $x265Approach.summary = "No: achieved bitrates are not similar (relative gap $([Math]::Round($gap, 4))); use a bitrate-matched x265 sweep for fairness."
        }
    }

    $bestCost = Select-BestBytes $costRows
    $details = if ($null -ne $bestCost) { $bestCost.details } else { $null }
    $partition = Get-Prop $details "partition"
    $total = if ($null -ne $bestCost) { [Int64]$bestCost.bytes } else { 0 }
    $interSyntaxMode = [string](Get-Prop $details "inter_syntax_mode")
    $actualMvBytes = switch ($interSyntaxMode) {
        "raw" { As-Int64 (Get-Prop $details "mv_raw_bytes_estimate") }
        "compact" { As-Int64 (Get-Prop $details "mv_compact_bytes") }
        "entropy" { As-Int64 (Get-Prop $details "mv_entropy_bytes") }
        default { As-Int64 (Get-Prop $details "mv_compact_bytes") }
    }
    $interResidual = As-Int64 (Get-Prop $details "inter_residual_bytes")
    $partitionMap = As-Int64 (Get-Prop $partition "partition_map_bytes")
    $partitionMv = As-Int64 (Get-Prop $partition "partition_mv_bytes")
    $partitionResidual = As-Int64 (Get-Prop $partition "partition_residual_bytes")
    $partitionHeader = As-Int64 (Get-Prop $partition "partition_header_bytes")
    $avgIBytes = As-Number (Get-Prop $details "avg_i_bytes")
    $keyframes = As-Int64 (Get-Prop $details "keyframes")
    $intraKeyframeCost = [Int64]([Math]::Round($avgIBytes * [double]$keyframes))
    $partitionSyntaxBytes = $partitionMap + $partitionMv
    $knownWithoutMvHeader = $interResidual + $partitionSyntaxBytes + $partitionHeader + $intraKeyframeCost
    $motionHeaderEstimate = (As-Int64 (Get-Prop $details "inter_header_bytes")) + $actualMvBytes
    $motionHeader = [Math]::Min([Math]::Max(0, $total - $knownWithoutMvHeader), $motionHeaderEstimate)
    $known = $motionHeader + $knownWithoutMvHeader
    $other = [Math]::Max(0, $total - $known)
    $buckets = @(
        [PSCustomObject]@{ name = "MV/header"; bytes = $motionHeader },
        [PSCustomObject]@{ name = "residual"; bytes = $interResidual },
        [PSCustomObject]@{ name = "partition syntax"; bytes = $partitionSyntaxBytes },
        [PSCustomObject]@{ name = "transform syntax"; bytes = $partitionHeader },
        [PSCustomObject]@{ name = "intra/keyframe cost"; bytes = $intraKeyframeCost },
        [PSCustomObject]@{ name = "poor prediction"; bytes = $other }
    )
    $biggestBottleneck = $buckets | Sort-Object @{ Expression = "bytes"; Ascending = $false }, @{ Expression = "name"; Ascending = $true } | Select-Object -First 1
    $nextFeature = Select-NextFeature $biggestBottleneck $x265Result $bestPartSyntaxV1 $bestPartSyntaxV2 $bModeAggregate

    $partSyntaxV2SmallerTotalCount = @($partSyntaxPairs | Where-Object { $_.v2_smaller_total }).Count
    $partSyntaxV2SmallerMapCount = @($partSyntaxPairs | Where-Object { $_.v2_smaller_map }).Count
    $autoFastV2SmallerTotalCount = @($partSyntaxPairs | Where-Object { $_.pair -eq "auto-fast-rdo" -and $_.v2_smaller_total }).Count
    $splitV2SmallerTotalCount = @($partSyntaxPairs | Where-Object { $_.pair -eq "split8x8" -and $_.v2_smaller_total }).Count
    $partSyntaxSummary = if ($autoFastV2SmallerTotalCount -gt 0) {
        "Yes: V2 reduced AutoFast RDO total bytes for at least one clip."
    } elseif ($splitV2SmallerTotalCount -gt 0) {
        "Partially: V2 reduced split8x8 total bytes and map bytes, but AutoFast RDO total bytes did not improve in this gate."
    } elseif ($partSyntaxV2SmallerMapCount -gt 0) {
        "Map-only: V2 reduced map bytes, but not adaptive partition total bytes in this gate."
    } else {
        "No: V2 did not reduce adaptive partition overhead in this gate."
    }

    $questions = [PSCustomObject]@{
        partition_syntax_v2_reduced_adaptive_overhead = [PSCustomObject]@{
            answered = ($partSyntaxPairs.Count -gt 0)
            total_pairs = $partSyntaxPairs.Count
            v2_smaller_total_count = $partSyntaxV2SmallerTotalCount
            v2_smaller_map_count = $partSyntaxV2SmallerMapCount
            auto_fast_v2_smaller_total_count = $autoFastV2SmallerTotalCount
            split8x8_v2_smaller_total_count = $splitV2SmallerTotalCount
            summary = $partSyntaxSummary
            pairs = $partSyntaxPairs
        }
        context_v1_reduced_bytes_vs_static_v1 = [PSCustomObject]@{
            answered = ($contextPairs.Count -gt 0)
            total_pairs = $contextPairs.Count
            context_smaller_count = @($contextPairs | Where-Object { $_.context_smaller }).Count
            summary = if (@($contextPairs | Where-Object { $_.context_smaller }).Count -gt 0) { "Yes: ContextV1 reduced total bytes for at least one clip." } else { "No: ContextV1 did not reduce total bytes vs StaticV1 in this gate." }
            pairs = $contextPairs
        }
        autofast_rdo_beat_fixed16x16_anywhere = [PSCustomObject]@{
            answered = ($autoFastPairs.Count -gt 0)
            total_pairs = $autoFastPairs.Count
            auto_fast_smaller_count = @($autoFastPairs | Where-Object { $_.auto_fast_rdo_smaller }).Count
            summary = if (@($autoFastPairs | Where-Object { $_.auto_fast_rdo_smaller }).Count -gt 0) { "Yes: AutoFast RDO beat fixed16x16 on at least one clip." } else { "No: AutoFast RDO did not beat fixed16x16 on total bytes in this gate." }
            pairs = $autoFastPairs
        }
        b_half_or_weighted_useful = [PSCustomObject]@{
            answered = ($bSummary.Count -gt 0)
            b_half_beats_b_int_count = $bModeAggregate.b_half_beats_b_int_count
            b_weighted_beats_b_int_count = $bModeAggregate.b_weighted_beats_b_int_count
            summary = if (($bModeAggregate.b_half_beats_b_int_count + $bModeAggregate.b_weighted_beats_b_int_count) -gt 0) { "Yes: at least one B-half or B-weighted row beat B-int." } else { "No: B-half and B-weighted did not beat B-int in this gate." }
            rows = $bSummary
        }
        srsv2_approached_x265_at_similar_bitrate_quality = $x265Approach
        biggest_byte_bottleneck = [PSCustomObject]@{
            category = $biggestBottleneck.name
            bytes = $biggestBottleneck.bytes
            source = if ($null -ne $bestCost) { "$($bestCost.clip)/$($bestCost.label)" } else { "" }
            buckets = $buckets
        }
    }

    $summary = [PSCustomObject]@{
        note = "Engineering measurement only; no H.265 superiority claim."
        out_root = $OutRoot
        seed = $Seed
        fps = $Fps
        qp = $Qp
        clips = $Clips
        ffmpeg = [PSCustomObject]@{ available = $encoders.ffmpeg; libx264 = $encoders.libx264; libx265 = $encoders.libx265 }
        best_static_v1_row = $bestStatic
        best_context_v1_row = $bestContext
        best_partition_syntax_v1_row = $bestPartSyntaxV1
        best_partition_syntax_v2_row = $bestPartSyntaxV2
        best_sweep_row = $bestSweep
        b_modes = $bModeAggregate
        x265 = $x265Result
        x265_status = $x265Status
        questions = $questions
        biggest_byte_bottleneck = [PSCustomObject]@{
            source = if ($null -ne $bestCost) { "$($bestCost.clip)/$($bestCost.label)" } else { "" }
            total_bytes = $total
            buckets = $buckets
            winner = $biggestBottleneck
        }
        next_feature = $nextFeature
    }
    $summary | ConvertTo-Json -Depth 32 | Set-Content -Path $SummaryJson

    $date = Get-Date -Format "yyyy-MM-dd HH:mm:ss K"
    $x265Line = if ($null -ne $x265Result) {
        "status=$($x265Result.status), bytes=$($x265Result.bytes), bitrate=$([Math]::Round($x265Result.x265_bitrate_bps, 2)), PSNR-Y=$([Math]::Round($x265Result.psnr_y, 4)), SSIM-Y=$([Math]::Round($x265Result.ssim_y, 4))"
    } else {
        $x265Status
    }
    $partSyntaxRowsMd = ($partSyntaxPairs | ForEach-Object {
        "| ``{0}`` | ``{1}`` | {2} | {3} | {4} | {5} | {6} | {7} |" -f $_.clip, $_.pair, $_.v1_total_bytes, $_.v2_total_bytes, $_.total_delta_v2_minus_v1, $_.v1_map_bytes, $_.v2_map_bytes, $_.map_delta_v2_minus_v1
    }) -join "`n"
    if ([string]::IsNullOrWhiteSpace($partSyntaxRowsMd)) {
        $partSyntaxRowsMd = "| _none_ | _none_ | 0 | 0 | 0 | 0 | 0 | 0 |"
    }
    $contextRowsMd = ($contextPairs | ForEach-Object {
        "| ``{0}`` | {1} | {2} | {3} |" -f $_.clip, $_.static_bytes, $_.context_bytes, $_.delta_context_minus_static
    }) -join "`n"
    if ([string]::IsNullOrWhiteSpace($contextRowsMd)) {
        $contextRowsMd = "| _none_ | 0 | 0 | 0 |"
    }
    $autoFastRowsMd = ($autoFastPairs | ForEach-Object {
        "| ``{0}`` | {1} | {2} | {3} |" -f $_.clip, $_.fixed16x16_bytes, $_.auto_fast_rdo_bytes, $_.delta_auto_minus_fixed
    }) -join "`n"
    if ([string]::IsNullOrWhiteSpace($autoFastRowsMd)) {
        $autoFastRowsMd = "| _none_ | 0 | 0 | 0 |"
    }
    $bRowsMd = ($bSummary | ForEach-Object {
        "| ``{0}`` | {1} | {2} | {3} | {4} |" -f $_.clip, $_.p_only_bytes, $_.b_int_bytes, $_.b_half_bytes, $_.b_weighted_bytes
    }) -join "`n"
    $bucketRowsMd = ($buckets | ForEach-Object {
        $share = if ($total -gt 0) { [Math]::Round(([double]$_.bytes / [double]$total), 4) } else { 0.0 }
        "| ``{0}`` | {1} | {2} |" -f $_.name, $_.bytes, $share
    }) -join "`n"

    $md = @"
# Windows HEVC Progress Gate Results

_Engineering measurement only. This report does **not** claim SRSV2 beats H.265/HEVC, x265, or any mature encoder._

## Run

- Date: $date
- Output root: ``var\bench\windows_hevc_progress\``
- Seed: $Seed; fps: $Fps; QP: $Qp
- Corpus: ``moving-square``, ``scrolling-bars``, ``checker``, ``scene-cut`` (64x64, 8 frames)
- FFmpeg available: **$($encoders.ffmpeg)**; libx264: **$($encoders.libx264)**; libx265: **$($encoders.libx265)**
- Commands: ``var\bench\windows_hevc_progress\commands_run.txt``

## Required Results

- Best StaticV1 row: $(Format-CandidateLine $bestStatic)
- Best ContextV1 row: $(Format-CandidateLine $bestContext)
- Best partition syntax v1 row: $(Format-CandidateLine $bestPartSyntaxV1)
- Best partition syntax v2 row: $(Format-CandidateLine $bestPartSyntaxV2)
- Best sweep row: $(Format-CandidateLine $bestSweep)
- Optional x265 result: $x265Line

## Questions Answered

### Did Partition Syntax V2 reduce adaptive partition overhead?

**$($questions.partition_syntax_v2_reduced_adaptive_overhead.summary)**

| Clip | Pair | v1 total bytes | v2 total bytes | Δ total (v2-v1) | v1 map bytes | v2 map bytes | Δ map (v2-v1) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
$partSyntaxRowsMd

### Did ContextV1 reduce bytes vs StaticV1?

**$($questions.context_v1_reduced_bytes_vs_static_v1.summary)**

| Clip | StaticV1 bytes | ContextV1 bytes | Δ context-static |
| --- | ---: | ---: | ---: |
$contextRowsMd

### Did AutoFast RDO beat fixed16x16 anywhere?

**$($questions.autofast_rdo_beat_fixed16x16_anywhere.summary)**

| Clip | fixed16x16 bytes | AutoFast RDO bytes | Δ auto-fixed |
| --- | ---: | ---: | ---: |
$autoFastRowsMd

### Did B-half or B-weighted become useful?

**$($questions.b_half_or_weighted_useful.summary)**

## B-Mode Results

| Clip | P-only bytes | B-int bytes | B-half bytes | B-weighted bytes |
| --- | ---: | ---: | ---: | ---: |
$bRowsMd

Counts: B-half smaller than B-int on **$($bModeAggregate.b_half_beats_b_int_count)** clips; B-weighted smaller than B-int on **$($bModeAggregate.b_weighted_beats_b_int_count)** clips.

### Did SRSV2 approach x265 at similar bitrate/quality?

**$($questions.srsv2_approached_x265_at_similar_bitrate_quality.summary)**

- SRSV2 bitrate (optional x265 clip): $($questions.srsv2_approached_x265_at_similar_bitrate_quality.srsv2_bitrate_bps)
- x265 bitrate: $($questions.srsv2_approached_x265_at_similar_bitrate_quality.x265_bitrate_bps)
- Similar bitrate (<=10% gap): **$(Format-YesNo $questions.srsv2_approached_x265_at_similar_bitrate_quality.similar_bitrate)**

## Biggest Byte Bottleneck

Source row: ``$($summary.biggest_byte_bottleneck.source)``; total bytes: **$total**.

| Bucket | Bytes | Share |
| --- | ---: | ---: |
$bucketRowsMd

Winner: **``$($biggestBottleneck.name)``** with **$($biggestBottleneck.bytes)** bytes.

## Next Feature

Exactly one next feature: **$($nextFeature.id). $($nextFeature.name)**.

Reason from this run: $($nextFeature.why)

Allowed feature set checked: A. CTU-style 64x64 superblocks; B. bounded quadtree partitions; C. context-adaptive residual coefficient entropy; D. quarter-pel luma motion; E. SAO-like restoration filter; F. 10-bit/HDR profile; G. bitrate-matched x265 sweep.

## Notes

- ``--compare-x265`` is optional and skipped when FFmpeg/libx265 is unavailable.
- ``--compare-x264-and-x265`` runs only when both encoders are reported by FFmpeg.
- x265 rows are CRF-style reference rows; they are **not** bitrate-matched proof.
- Full machine-readable summary: ``var\bench\windows_hevc_progress\summary.json``.
"@
    Set-Content -Path $SummaryMd -Value $md
    Set-Content -Path $DocsMd -Value $md

    Write-Host "HEVC progress summary written:"
    Write-Host "  JSON: $SummaryJson"
    Write-Host "  MD:   $SummaryMd"
    Write-Host "  DOC:  $DocsMd"
    Write-Host "  next_feature: $($nextFeature.id) $($nextFeature.name)"
}
finally {
    Pop-Location
}
