# Windows HEVC Progress Gate Results

_Engineering measurement only. This report does **not** claim SRSV2 beats H.265/HEVC, x265, or any mature encoder._

## Run

- Date: 2026-05-08 21:35:39 -06:00
- Output root: `var\bench\windows_hevc_progress\`
- Seed: 528; fps: 30; QP: 28
- Corpus: `moving-square`, `scrolling-bars`, `checker`, `scene-cut` (64x64, 8 frames)
- FFmpeg available: **True**; libx264: **True**; libx265: **True**
- Commands: `var\bench\windows_hevc_progress\commands_run.txt`

## Required Results

- Best StaticV1 row: clip=scene_cut, label=SRSV2-entropy-StaticV1, mode=static, bytes=4956, PSNR-Y=13.3004, SSIM-Y=0.6935
- Best ContextV1 row: clip=scene_cut, label=SRSV2-entropy-ContextV1, mode=context, bytes=4961, PSNR-Y=13.3004, SSIM-Y=0.6935
- Best partition syntax v1 row: clip=scene_cut, label=fixed16x16, mode=v1, bytes=4949, PSNR-Y=13.3004, SSIM-Y=0.6935
- Best partition syntax v2 row: clip=moving_square, label=auto-fast-rdo-v2, mode=v2, bytes=8523, PSNR-Y=15.7806, SSIM-Y=0.7528
- Best sweep row: clip=moving_square, label=row_19, mode=entropy/static/rdo-fast/auto-fast, bytes=8360, PSNR-Y=15.7482, SSIM-Y=0.7538
- Optional x265 result: status=ok, bytes=3561, bitrate=106830, PSNR-Y=100, SSIM-Y=1

## Questions Answered

### Did Partition Syntax V2 reduce adaptive partition overhead?

**Partially: V2 reduced split8x8 total bytes and map bytes, but AutoFast RDO total bytes did not improve in this gate.**

| Clip | Pair | v1 total bytes | v2 total bytes | Δ total (v2-v1) | v1 map bytes | v2 map bytes | Δ map (v2-v1) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `moving_square` | `auto-fast-rdo` | 8347 | 8523 | 176 | 112 | 0 | -112 |
| `moving_square` | `split8x8` | 8806 | 8792 | -14 | 112 | 0 | -112 |
| `scrolling_bars` | `auto-fast-rdo` | 18418 | 18630 | 212 | 112 | 0 | -112 |
| `scrolling_bars` | `split8x8` | 18936 | 18922 | -14 | 112 | 0 | -112 |
| `checker` | `auto-fast-rdo` | 19489 | 19659 | 170 | 112 | 0 | -112 |
| `checker` | `split8x8` | 20043 | 20029 | -14 | 112 | 0 | -112 |
| `scene_cut` | `auto-fast-rdo` | 12229 | 12293 | 64 | 112 | 0 | -112 |
| `scene_cut` | `split8x8` | 12829 | 12815 | -14 | 112 | 0 | -112 |

### Did ContextV1 reduce bytes vs StaticV1?

**No: ContextV1 did not reduce total bytes vs StaticV1 in this gate.**

| Clip | StaticV1 bytes | ContextV1 bytes | Δ context-static |
| --- | ---: | ---: | ---: |
| `moving_square` | 6591 | 6593 | 2 |
| `scrolling_bars` | 8975 | 8976 | 1 |
| `checker` | 16791 | 16792 | 1 |
| `scene_cut` | 4956 | 4961 | 5 |

### Did AutoFast RDO beat fixed16x16 anywhere?

**No: AutoFast RDO did not beat fixed16x16 on total bytes in this gate.**

| Clip | fixed16x16 bytes | AutoFast RDO bytes | Δ auto-fixed |
| --- | ---: | ---: | ---: |
| `moving_square` | 6566 | 8347 | 1781 |
| `scrolling_bars` | 8973 | 18418 | 9445 |
| `checker` | 16761 | 19489 | 2728 |
| `scene_cut` | 4949 | 12229 | 7280 |

### Did B-half or B-weighted become useful?

**No: B-half and B-weighted did not beat B-int in this gate.**

## B-Mode Results

| Clip | P-only bytes | B-int bytes | B-half bytes | B-weighted bytes |
| --- | ---: | ---: | ---: | ---: |
| `moving_square` | 6783 | 6500 | 6955 | 6900 |
| `scrolling_bars` | 9190 | 9938 | 10247 | 10303 |
| `checker` | 16978 | 12382 | 13067 | 12766 |
| `scene_cut` | 5166 | 5418 | 5778 | 5802 |

Counts: B-half smaller than B-int on **0** clips; B-weighted smaller than B-int on **0** clips.

### Did SRSV2 approach x265 at similar bitrate/quality?

**No: achieved bitrates are not similar (relative gap 0.475); use a bitrate-matched x265 sweep for fairness.**

- SRSV2 bitrate (optional x265 clip): 203490
- x265 bitrate: 106830
- Similar bitrate (<=10% gap): **no**

## Biggest Byte Bottleneck

Source row: `scene_cut/SRSV2-pc-fixed16x16`; total bytes: **4949**.

| Bucket | Bytes | Share |
| --- | ---: | ---: |
| `MV/header` | 294 | 0.0594 |
| `residual` | 4058 | 0.82 |
| `partition syntax` | 0 | 0 |
| `transform syntax` | 0 | 0 |
| `intra/keyframe cost` | 597 | 0.1206 |
| `poor prediction` | 0 | 0 |

Winner: **`residual`** with **4058** bytes.

## Next Feature

Exactly one next feature: **C. context-adaptive residual coefficient entropy**.

Reason from this run: Inter residual bytes are the largest named bucket.

Allowed feature set checked: A. CTU-style 64x64 superblocks; B. bounded quadtree partitions; C. context-adaptive residual coefficient entropy; D. quarter-pel luma motion; E. SAO-like restoration filter; F. 10-bit/HDR profile; G. bitrate-matched x265 sweep.

## Notes

- `--compare-x265` is optional and skipped when FFmpeg/libx265 is unavailable.
- `--compare-x264-and-x265` runs only when both encoders are reported by FFmpeg.
- x265 rows are CRF-style reference rows; they are **not** bitrate-matched proof.
- Full machine-readable summary: `var\bench\windows_hevc_progress\summary.json`.
