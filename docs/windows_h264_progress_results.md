# Windows H.264 Progress Gate Results

_Engineering measurement only. This report does not claim SRSV2 beats H.264._

## Run

- Date: 2026-05-04
- OS: Windows (`windows-x86_64` in report JSON)
- Git (from bench JSON): `ec399b6`
- Seed: 528; fps: 30; QP: 28; keyint: 6; motion radius: 4; residual entropy: `auto`
- Clips: `moving_square`, `scrolling_bars`, `checker`, `scene_cut` (32×32, 6 frames each)
- Machine output root: `var/bench/windows_h264_progress/`

## Command list

Primary gate (from `tools/windows_h264_progress_baseline.ps1`):

```powershell
powershell -ExecutionPolicy Bypass -File tools\windows_h264_progress_baseline.ps1
```

Executed commands are logged verbatim in `var/bench/windows_h264_progress/commands_run.txt`. Summary:

1. `cargo build --release -p quality_metrics --bin gen_synthetic_yuv --bin bench_srsv2`
2. Per clip: `gen_synthetic_yuv --pattern <pattern> --width 32 --height 32 --frames 6 --fps 30 --seed 528 --out ... --meta ...`
3. Per clip: `bench_srsv2 ... --inter-syntax entropy --compare-entropy-models --report-json ... --report-md ...`
4. Per clip: `bench_srsv2 ... --inter-syntax compact --compare-partition-costs --report-json ... --report-md ...`
5. Per clip: `bench_srsv2 ... --sweep-quality-bitrate --report-json ... --report-md ...`
6. Per clip: `bench_srsv2 ... --compare-b-modes --reference-frames 2 --bframes 1 --report-json ... --report-md ...`
7. `moving_square` only (if `ffmpeg` on PATH): `bench_srsv2 ... --compare-x264 --report-json ... --report-md ...`
8. `bench_srsv2 --h264-progress-summary --entropy-models-json ... --partition-costs-json ... --sweep-quality-bitrate-json ... --compare-b-modes-json ... [--compare-x264-json ...] --progress-summary-json ... --progress-summary-md ...`

## Best StaticV1 row

Source: `var/bench/windows_h264_progress/reports/moving_square/compare_entropy_models.json` (progress summary inputs use `moving_square`).

| Field | Value |
| --- | ---: |
| Row | `SRSV2-entropy-StaticV1` |
| Total bytes | 607 |
| PSNR-Y | 24.7927 |
| SSIM-Y | 0.9177 |
| Bitrate bps | 24280 |
| Static MV entropy section bytes | 88 |
| Inter header bytes (details) | 138 |
| Inter residual bytes (details) | 304 |

## Best ContextV1 row

| Field | Value |
| --- | ---: |
| Row | `SRSV2-entropy-ContextV1` |
| Total bytes | 607 |
| PSNR-Y | 24.7927 |
| SSIM-Y | 0.9177 |
| Bitrate bps | 24280 |
| Context MV entropy section bytes | 88 |

Result: **tie** — Δ(context − static) = **0** bytes on this gate (`summary.json` questions.context_v1_vs_static_v1_bytes).

## Best partition-cost row

Source: `compare_partition_costs.json` for `moving_square`.

**Smallest total bytes** (best byte row in the compare table):

| Field | Value |
| --- | ---: |
| Row | `SRSV2-pc-fixed16x16` |
| Bytes | 559 |
| PSNR-Y | 24.7927 |
| SSIM-Y | 0.9177 |

Auto-fast variants on the same clip (same JSON `table`):

| Row | Bytes | SSIM-Y |
| --- | ---: | ---: |
| `SRSV2-pc-auto-fast-sad` | 837 | 0.9117 |
| `SRSV2-pc-auto-fast-header-aware` | 769 | 0.9243 |
| `SRSV2-pc-auto-fast-rdo` | 769 | 0.9243 |
| `SRSV2-pc-split8x8` | 901 | 0.9205 |

Progress-summary facts (`summary.json`): `partition_rejected_by_rdo` = **2**, `partition_rejected_by_header_cost` = **0**; auto-fast RDO bytes **769** vs sad-only **837** (RDO same or smaller than sad: **true**). Fixed16×16 still uses **210 fewer bytes** than auto-fast RDO on this clip (559 vs 769).

## Best quality / bitrate sweep row

Source: `var/bench/windows_h264_progress/reports/moving_square/sweep_quality_bitrate.json` (60 rows).

**Highest SSIM-Y** among `ok: true` rows:

| Field | Value |
| --- | --- |
| `row_index` | 15 |
| QP | 22 |
| `inter_syntax` | `compact` |
| `entropy_model` | `static` |
| `partition_cost_model` | `rdo-fast` |
| `inter_partition` | `auto-fast` |
| Total bytes | 789 |
| PSNR-Y | 24.9662 |
| SSIM-Y | **0.9297** |

Sweep decision statistic from `summary.json`: **30** comparable slices; auto-fast **smaller total_bytes than fixed16×16: 0** (ties possible).

## B-int vs B-half vs B-weighted

Per-clip totals from each `reports/<clip>/compare_b_modes.json` (`table` codec rows):

| Clip | P-only | B-int | B-half | B-weighted | Note |
| --- | ---: | ---: | ---: | ---: | --- |
| `moving_square` | 594 | 750 | 768 | 818 | B-int +156 vs P-only; half +18 vs int; weighted +68 vs int. |
| `scrolling_bars` | 1855 | 1973 | 2001 | 2034 | P-only smallest; B modes larger. |
| `checker` | 2938 | 2328 | 2471 | 2392 | B-int smallest overall (2328 vs P-only 2938); B-half and weighted did not beat B-int. |
| `scene_cut` | 1343 | 1390 | 1492 | 1454 | B-int smallest among B modes; all B rows ≥ P-only on this clip. |

`moving_square` (summary clip): half-pel and weighted paths **did not reduce** bytes versus B-int.

## Optional x264 result (FFmpeg present)

Source: `reports/moving_square/compare_x264.json` (`x264.status`: **ok**).

| Codec | Bytes | Bitrate bps | PSNR-Y | SSIM-Y |
| --- | ---: | ---: | ---: | ---: |
| SRSV2 | 594 | 23760 | 24.7927 | 0.9177 |
| x264 | 1708 | 68320 | 100.0 | 1.0 |

Encoder line from JSON: `libx264`, preset **medium**, **CRF 23** (not QP-matched to SRSV2’s fixed-QP path). Metrics are **not** claimed comparable; this row only proves the optional tool ran.

## Biggest byte bottleneck

From `summary.json` → `byte_cost_breakdown` (source row `SRSV2-pc-auto-fast-rdo`, total **769** bytes):

| Bucket | Bytes | Share of row |
| --- | ---: | ---: |
| MV/header (`mv_header_bytes`) | 48 | 0.0624 |
| Inter residual (`inter_residual_bytes`) | 0 | 0.0000 |
| Partition map | 20 | 0.0260 |
| Transform / partition header syntax | 20 | 0.0260 |
| `poor_prediction_proxy` (row total minus summed buckets) | 681 | 0.8856 |

`next_bottleneck`: **`poor_prediction_proxy`**. Rationale in JSON: largest share is payload not yet mapped into the listed buckets (containers, slice headers, other syntax) until telemetry is extended.

## Next recommended codec feature

**Choice: C — Partition syntax redesign** (per gate rules: partition / map / MV side still wins for simple partition, and auto-fast does not beat fixed16×16 on bytes).

Evidence tied to numbers above:

- **C trigger:** In **30** comparable sweep slices, auto-fast was **never** smaller than fixed16×16 on `total_bytes` (`summary.json` `auto_fast_vs_fixed16_in_sweep`). On `moving_square`, `SRSV2-pc-fixed16x16` is **559** bytes vs **`SRSV2-pc-auto-fast-rdo` 769** bytes (+210 bytes for adaptive partition path on this clip).
- **Not A (quarter-pel luma motion):** B-half **768** vs B-int **750** on `moving_square`—extra fractional MV work **increased** bytes here; gate does not show “half-pel helped quality without exploding bytes” at the bit accounting level.
- **Not B (context-adaptive residual coefficient entropy):** ContextV1 **tied** StaticV1 on total bytes (**607** each); it did not establish a residual/MV-byte win for the next move.
- **Not D (better intra prediction):** Single keyframe is part of cost (e.g. **165** B avg I-bytes in entropy details) but the summary bottleneck and sweep partition gap point to **inter partition / syntax accounting**, not an isolated I-frame dominance claim from this gate.
- **Not E (bitrate-matched x264 sweep):** Payload ratio SRSV2/x264 = **594/1708 ≈ 0.348** on this optional row, with **different** rate control (CRF vs fixed QP). Measurement fairness is **not** the only remaining issue from these numbers.

Exactly **one** next feature: **C**.
