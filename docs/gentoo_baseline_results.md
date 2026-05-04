# Gentoo baseline benchmark results

_Engineering measurements only — not a codec superiority claim. Numbers are machine-specific._

## Host snapshot

```
Linux customer.dnvrcox1.isp.starlink.com 6.18.18-gentoo-dist #1 SMP PREEMPT_DYNAMIC Fri Mar 13 19:40:36 -00 2026 x86_64 Intel(R) Core(TM) i7-7700 CPU @ 3.60GHz GenuineIntel GNU/Linux
rustc: rustc 1.92.0 (ded5c06cf 2025-12-08)
cargo: cargo 1.92.0 (344c4567c 2025-10-21)
ffmpeg: ffmpeg version 8.0.1 Copyright (c) 2000-2025 the FFmpeg developers
XDG_SESSION_TYPE=wayland
```

## Commands

```bash
bash tools/gentoo_bench_baseline.sh
```

Optional: `GENTOO_BASELINE_SEED=528`, `GENTOO_BASELINE_SKIP_BUILD=1` if `bench_srsv2` already built.

## Best SRSV2 rows

See [`var/bench/gentoo_baseline/SUMMARY.md`](../var/bench/gentoo_baseline/SUMMARY.md) (generated; under `var/` — gitignored except structure described here).

### Copy of summary table

| Clip | compare-inter-syntax | compare-rdo | compare-partition-costs | compare-entropy-models | sweep-quality-bitrate | compare-x264 |
|------|----------------------|-------------|-------------------------|------------------------|---------------------|---------------|
| `gentoo_flat_64x64` | 1422 bytes — SRSV2-entropy | 1932 bytes — SRSV2-rdo-off | 1467 bytes — SRSV2-pc-fixed16x16 | 1326 bytes — SRSV2 entropy static | 1422 bytes — qp=18 inter=entropy part=fixed16x16 pcm=header-aware | 1932 bytes — SRSV2 |
| `gentoo_gradient_64x64` | 4126 bytes — SRSV2-compact | 4588 bytes — SRSV2-rdo-off | 3862 bytes — SRSV2-pc-auto-fast-header-aware | 3793 bytes — SRSV2 entropy static | 3770 bytes — qp=26 inter=entropy part=auto-fast pcm=header-aware | 4588 bytes — SRSV2 |
| `gentoo_moving_square_128x128` | 8953 bytes — SRSV2-entropy | 12868 bytes — SRSV2-rdo-fast | 10054 bytes — SRSV2-pc-fixed16x16 | 8323 bytes — SRSV2 entropy context | 8913 bytes — qp=22 inter=entropy part=fixed16x16 pcm=header-aware | 12944 bytes — SRSV2 |
| `gentoo_scrolling_bars_128x128` | 49564 bytes — SRSV2-entropy | 52875 bytes — SRSV2-rdo-fast | 50093 bytes — SRSV2-pc-fixed16x16 | 45804 bytes — SRSV2 entropy context | 47924 bytes — qp=26 inter=entropy part=fixed16x16 pcm=header-aware | 52884 bytes — SRSV2 |
| `gentoo_checker_64x64` | 7240 bytes — SRSV2-compact | 7692 bytes — SRSV2-rdo-off | 7240 bytes — SRSV2-pc-fixed16x16 | 6572 bytes — SRSV2 entropy context | 7238 bytes — qp=30 inter=compact part=fixed16x16 pcm=header-aware | 7692 bytes — SRSV2 |
| `gentoo_scene_cut_128x128` | 28812 bytes — SRSV2-entropy | 32932 bytes — SRSV2-rdo-off | 30083 bytes — SRSV2-pc-fixed16x16 | 26124 bytes — SRSV2 entropy static | 28750 bytes — qp=22 inter=entropy part=fixed16x16 pcm=header-aware | 32932 bytes — SRSV2 |

## Artifact paths

Corpus: `/home/c0ldfyr3/DEV/.528/var/bench/gentoo_baseline/corpus`
Runs: `/home/c0ldfyr3/DEV/.528/var/bench/gentoo_baseline/runs`

## Sweep parameters

- `--sweep-quality-bitrate` with `--sweep-ssim-threshold 0.90` and `--sweep-byte-budget 100000000`

## Disclaimer

See [`docs/srsv2_benchmarks.md`](srsv2_benchmarks.md) for methodology. Optional x264 rows require `ffmpeg` on `PATH`.

## RDO note (encoder tooling)

**AutoFast + `RdoFast`** partition decisions now include estimated **partition-map / transform-syntax / block-AQ** side bytes (see `libsrs_video::srsv2::rdo::autofast_partition_mb_rdo_score`) instead of MV+residual length only. Numbers in this file stay tied to the **run date** above; after pulling encoder changes, re-run `bash tools/gentoo_bench_baseline.sh` (or `--only-summary` if only regenerating markdown from existing `runs/`).
