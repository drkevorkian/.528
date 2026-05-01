# SRSV2 measurement methodology (optional comparisons)

Workspace tools (engineering measurements):

- Generate deterministic YUV420p8 clips:
  - `cargo run -p quality_metrics --bin gen_synthetic_yuv -- --pattern moving-square --width 1920 --height 1080 --frames 60 --fps 60 --seed 528 --out samples/bench/moving_square_1080p.yuv --meta samples/bench/moving_square_1080p.json`
- Benchmark SRSV2 core (optional x264 via ffmpeg/libx264):
  - `cargo run -p quality_metrics --bin bench_srsv2 -- --input samples/bench/moving_square_1080p.yuv --width 1920 --height 1080 --frames 60 --fps 60 --qp 28 --keyint 30 --motion-radius 16 --report-json var/bench/moving_square_srsv2.json --report-md var/bench/moving_square_srsv2.md`
  - Add `--compare-x264 --x264-crf 23 --x264-preset medium` if `ffmpeg` is on `PATH`.

Legacy helper: `cargo run -p codec_compare -- --help` (optional **libx264** branch via `ffmpeg`).

This file describes **reproducible** measurement practices when you compare SRSV2 to **other** video encoders (for example a common **AVC** baseline). It is **not** a scorecard and implies **no** ranking — quality trade-offs are for **you** to judge.

## Fair comparison checklist

1. **Baseline encoder (optional):** e.g. **libx264** via **FFmpeg** (or another AVC encoder with documented settings) on the same machine class as SRSV2.
2. **Fair comparison:**
   - Same **resolution**, **frame count**, **chroma format** (or document conversions).
   - Same or documented **color range** / **transfer** when HDR is involved.
   - **Bitrate** matched either by **two-pass** targeting bitrate or by **CRF** with reported achieved bitrate for both sides.
3. **SRSV2 side:** documented **preset**, **profile byte**, **QP/keyframe** settings, commit hash.
4. **Metrics** (report all that apply):
   - **Bitrate** (bits/s) and **compression ratio** vs uncompressed PCM/YUV size.
   - **PSNR** (luma and optionally weighted).
   - **SSIM** / **MS-SSIM** (when tooling agrees on window/color space).
   - **VMAF** (optional; requires `vmaf`/`ffmpeg` with **libvmaf** or Netflix VMAF CLI).
   - **Encode FPS** and **decode FPS** (single-thread vs multi-thread noted).

## Suggested FFmpeg skeleton (AVC baseline)

Exact flags evolve with test vectors; this is a **template**:

```bash
ffmpeg -y -f rawvideo -pix_fmt yuv420p -s WIDTHxHEIGHT -r FRAMERATE -i input.yuv \
  -c:v libx264 -preset medium -crf Q -an out264.mp4
```

Extract throughput from **`ffmpeg` stderr** or wrap with `time` / perf counters. Decode throughput:

```bash
ffmpeg -benchmark -i out264.mp4 -f null -
```

## SRSV2 side

Use **`srs_cli`** / **`libsrs_video`** encode paths with pinned settings; decode via **`PlaybackSession`** or standalone SRSV2 decode utilities. Record **wall time** and **CPU time** where possible.

## CI policy

- Optional job: run short clips on **schedule** or **manual workflow** (VMs without FFmpeg skip gracefully).
- Do **not** fail default PR CI on VMAF absence; gate VMAF behind feature detection.

## Reporting template

| Field | Example |
|-------|---------|
| Date / commit | |
| Hardware / OS | |
| Resolution / fps / frames | |
| SRSV2 preset / profile / QP | |
| Baseline encoder preset / CRF / bitrate | |
| Bitrate SRSV2 / baseline | |
| PSNR / SSIM / MS-SSIM / VMAF | |
| Encode FPS / decode FPS | |
