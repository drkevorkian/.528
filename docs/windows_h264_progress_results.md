# Windows H.264 Progress Gate Results

_Engineering measurement only. This report does not claim SRSV2 beats H.264._

## Run

- Date: 2026-05-04
- OS: Windows
- Seed: 528
- Clips: `moving-square`, `scrolling-bars`, `checker`, `scene-cut`
- Geometry: 32x32, 6 frames, 30 fps
- QP: 28
- Keyint: 6
- Motion radius: 4
- Output root: `var/bench/windows_h264_progress/`

## Command List

Primary gate command:

```powershell
powershell -ExecutionPolicy Bypass -File tools\windows_h264_progress_baseline.ps1
```

The script ran these command classes for each clip:

- `cargo build --release -p quality_metrics --bin gen_synthetic_yuv --bin bench_srsv2`
- `gen_synthetic_yuv --pattern <pattern> --width 32 --height 32 --frames 6 --fps 30 --seed 528 --out ... --meta ...`
- `bench_srsv2 --inter-syntax entropy --compare-entropy-models`
- `bench_srsv2 --inter-syntax compact --compare-partition-costs`
- `bench_srsv2 --sweep-quality-bitrate`
- `bench_srsv2 --compare-b-modes --reference-frames 2 --bframes 1`
- `bench_srsv2 --compare-x264` on `moving_square` because FFmpeg was available
- `bench_srsv2 --h264-progress-summary --entropy-models-json ... --partition-costs-json ... --sweep-quality-bitrate-json ... --compare-b-modes-json ... --compare-x264-json ... --progress-summary-json ... --progress-summary-md ...`

Full expanded command log: `var/bench/windows_h264_progress/commands_run.txt`.

## Best Entropy Rows

Best StaticV1 row by total bytes:

- Clip: `moving_square`
- Row: `SRSV2-entropy-StaticV1`
- Bytes: 607
- PSNR-Y: 24.793
- SSIM-Y: 0.9177
- Static MV bytes: 88
- MV deltas: zero 18, nonzero 22, avg abs 8.2

Best ContextV1 row by total bytes:

- Clip: `moving_square`
- Row: `SRSV2-entropy-ContextV1`
- Bytes: 607
- PSNR-Y: 24.793
- SSIM-Y: 0.9177
- Context MV bytes: 88
- MV deltas: zero 18, nonzero 22, avg abs 8.2

Result: ContextV1 tied StaticV1 on the best byte row. It did not reduce bytes on this gate run.

## Best Partition-Cost Row

Best partition-cost row by total bytes:

- Clip: `moving_square`
- Row: `SRSV2-pc-fixed16x16`
- Bytes: 559
- PSNR-Y: 24.793
- SSIM-Y: 0.9177

Progress-summary source row (`moving_square`, `SRSV2-pc-auto-fast-rdo`):

- Auto-fast RDO bytes: 781
- Auto-fast sad-only bytes: 837
- RDO partition rejects: 4
- Header-cost rejects: 0
- RDO same or smaller than sad-only: true

Result: RDO reduced the auto-fast sad-only cost, but fixed16x16 still beat auto-fast-rdo by 222 bytes on the summary clip.

## Best Quality / Bitrate Sweep Row

Best row by quality in the generated sweep:

- Clip: `moving_square`
- QP: 22
- Inter syntax: `compact`
- Entropy model: `static`
- Partition cost model: `rdo-fast`
- Inter partition: `auto-fast`
- Bytes: 789
- PSNR-Y: 24.966
- SSIM-Y: 0.9297

Auto-fast vs fixed16x16 sweep result from the progress summary:

- Comparable slices: 30
- Auto-fast smaller-byte wins: 0

## B-Mode Result

| Clip | P-only bytes | B-int bytes | B-half bytes | B-weighted bytes | Result |
|---|---:|---:|---:|---:|---|
| `checker` | 2938 | 2328 | 2471 | 2392 | B-int was smallest among B modes; B-half and weighted did not pay. |
| `moving_square` | 594 | 750 | 768 | 818 | P-only was smallest; B-half and weighted did not pay. |
| `scene_cut` | 1343 | 1390 | 1492 | 1454 | P-only was smallest; B-half and weighted did not pay. |
| `scrolling_bars` | 1855 | 1973 | 2001 | 2034 | P-only was smallest; B-half and weighted did not pay. |

Progress-summary B result for `moving_square`: B-int 750 bytes, B-half 768 bytes, B-weighted 818 bytes.

## Optional x264 Result

FFmpeg was available, so the script ran the optional x264 report on `moving_square`.

| Codec | Bytes | Bitrate bps | PSNR-Y | SSIM-Y |
|---|---:|---:|---:|---:|
| SRSV2 | 594 | 23760 | 24.793 | 0.9177 |
| x264 | 1708 | 68320 | 100.000 | 1.0000 |

This is not bitrate-matched. It is only a local reference row showing that the optional path works.

## Biggest Byte Bottleneck

The progress summary used the `moving_square` report set and named:

- Biggest bottleneck: `poor_prediction_proxy`
- Source row: `SRSV2-pc-auto-fast-rdo`
- Total row bytes: 781
- MV/header bytes: 60 (7.68%)
- Inter residual bytes: 0 (0.00%)
- Partition map bytes: 20 (2.56%)
- Transform syntax bytes: 20 (2.56%)
- Other / unbucketed proxy bytes: 681 (87.20%)

Interpretation: the currently instrumented byte buckets do not explain most of the auto-fast-rdo partition row. Given fixed16x16 wins the partition-cost compare and auto-fast has zero smaller-byte wins in 30 sweep slices, the immediate blocker is the partitioned syntax/decision path rather than ContextV1 MV entropy or B-half/weighted prediction.

## Decision

Chosen next codec feature: **C. Partition syntax redesign**.

Evidence:

- Auto-fast-rdo did not beat fixed16x16 on the best partition-cost row: 781 bytes vs 559 bytes on `moving_square`.
- Auto-fast had 0 smaller-byte wins across 30 comparable quality/bitrate sweep slices.
- B-half and weighted B did not pay in any clip in this gate.
- ContextV1 tied StaticV1 on the best entropy row: both 607 bytes, both 88 MV entropy bytes.
- The summary bottleneck is `poor_prediction_proxy` at 681 / 781 bytes on the auto-fast-rdo row, which points to unaccounted partitioned syntax/overhead before adding a new prediction tool.

Do not choose A yet: half-pel did not help without byte growth in this gate.

Do not choose B yet: residual bytes were not the named summary bottleneck, and ContextV1 did not reduce MV/header bytes.

Do not choose D yet: this gate did not isolate I-frame/keyframe cost as the largest issue.

Do not choose E yet: the x264 row was optional and not bitrate-matched; fairness still needs a separate bitrate-matched run, but these numbers do not make it the immediate blocker.
