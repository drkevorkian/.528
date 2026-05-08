# Windows HEVC Progress Gate Results

_Engineering measurement only. This report does **not** claim SRSV2 beats H.265/HEVC, x265, or any mature encoder._

## Run

- Date: 2026-05-07 20:23:30 -06:00
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

## B-Mode Results

| Clip | P-only bytes | B-int bytes | B-half bytes | B-weighted bytes |
| --- | ---: | ---: | ---: | ---: |
| `moving_square` | 6783 | 6500 | 6955 | 6900 |
| `scrolling_bars` | 9190 | 9938 | 10247 | 10303 |
| `checker` | 16978 | 12382 | 13067 | 12766 |
| `scene_cut` | 5166 | 5418 | 5778 | 5802 |

Counts: B-half smaller than B-int on **0** clips; B-weighted smaller than B-int on **0** clips.

## Biggest Byte Bottleneck

Source row: `scene_cut/SRSV2-pc-fixed16x16`; total bytes: **4949**.

| Bucket | Bytes | Share |
| --- | ---: | ---: |
| `motion_header_bytes` | 966 | 0.1952 |
| `inter_residual_bytes` | 4058 | 0.82 |
| `partition_map_bytes` | 0 | 0 |
| `partition_mv_bytes` | 0 | 0 |
| `partition_residual_bytes` | 0 | 0 |
| `other_payload_bytes` | 0 | 0 |

Winner: **`inter_residual_bytes`** with **4058** bytes.

## Next Feature

Exactly one next feature: **C. context-adaptive residual coefficient entropy**.

Reason from this run: Inter residual bytes are the largest named bucket.

Allowed feature set checked: A. CTU-style 64x64 superblocks; B. bounded quadtree partitions; C. context-adaptive residual coefficient entropy; D. quarter-pel luma motion; E. SAO-like restoration filter; F. 10-bit/HDR profile; G. bitrate-matched x265 sweep.

## Notes

- `--compare-x265` is optional and skipped when FFmpeg/libx265 is unavailable.
- `--compare-x264-and-x265` runs only when both encoders are reported by FFmpeg.
- x265 rows are CRF-style reference rows; they are **not** bitrate-matched proof.
- Full machine-readable summary: `var\bench\windows_hevc_progress\summary.json`.
