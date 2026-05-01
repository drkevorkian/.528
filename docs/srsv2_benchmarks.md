# SRSV2 measurement methodology (optional comparisons)

Workspace tools (engineering measurements):

- Generate deterministic YUV420p8 clips (`--out`, `--meta`; patterns include `flat`, `gray-ramp`, `moving-square`, `checker`, `noise`, `scene-cut`):
  - `cargo run -p quality_metrics --bin gen_synthetic_yuv -- --pattern moving-square --width 1920 --height 1080 --frames 60 --fps 60 --seed 528 --out samples/bench/moving_square_1080p.yuv --meta samples/bench/moving_square_1080p.json`
- Benchmark SRSV2 core (**no FFmpeg required**; optional x264 via ffmpeg/libx264):
  - `cargo run -p quality_metrics --bin bench_srsv2 -- --input samples/bench/moving_square_1080p.yuv --width 1920 --height 1080 --frames 60 --fps 60 --qp 28 --keyint 30 --motion-radius 16 --residual-entropy auto --report-json var/bench/moving_square_srsv2.json --report-md var/bench/moving_square_srsv2.md`
  - Add `--compare-x264 --x264-crf 23 --x264-preset medium` if `ffmpeg` is on `PATH`.
  - Residual coding: `--residual-entropy auto|explicit|rans`. **`auto`** never chooses rANS for a block when that would be larger than explicit tuples (unless forced **`rans`**). Reports include intra/P **explicit vs rANS** counts and optional **`legacy_explicit_total_payload_bytes`** when not `explicit`.
- **Compare residual modes** (single command, three encode passes — **no FFmpeg**): `--compare-residual-modes` produces rows **SRSV2-explicit**, **SRSV2-auto**, **SRSV2-rans**. If forced **rans** fails (e.g. coefficients outside the static rANS alphabet), that row is marked failed with an error string and the other rows still appear.
- **Rate control** (benchmark loop only; see `docs/rate_control.md`): `--rc fixed-qp|quality|target-bitrate` with `--quality`, `--target-bitrate-kbps`, optional `--max-bitrate-kbps`, and `--min-qp` / `--max-qp` / `--qp-step-limit`. Reports include achieved vs target bitrate and QP history summaries.
- **Sweep grid** (optional regression / weak-spot finder): `--sweep` runs a fixed grid of QP values `{18, 22, 28, 34}` × residual `{explicit, auto}` × motion radius `{0, 8, 16}` and writes a JSON array plus a Markdown table (`--compare-residual-modes` and `--sweep` are mutually exclusive).

Legacy helper: `cargo run -p codec_compare -- --help` (optional **libx264** branch via `ffmpeg`).

This file describes **reproducible** measurement practices when you compare SRSV2 to **other** video encoders (for example a common **AVC** baseline). It is **not** a scorecard and implies **no** ranking — quality trade-offs are for **you** to judge. This is a **compression engineering** step for the native codec, **not** a claim about beating H.264 or any other standard encoder.

## Example: 128×128, 30 frames, `flat` pattern (auto residual)

Command (after generating `var/bench/flat_128.yuv` / `.json` as in the snippets above with `--pattern flat --width 128 --height 128`):

- SRSV2 payload **~11.9 KiB** for the clip, **1** keyframe / **29** P-frames, **`avg_i_bytes` ~2325**, **`avg_p_bytes` ~329** (one lab run on Windows; numbers vary by CPU and build).
- **`intra_rans_blocks` 0** / **`intra_explicit_blocks` 384** for this flat clip: every **8×8** luma/chroma block stayed on **explicit** AC tuples under **`auto`** (rANS is not smaller here).
- **`legacy_explicit_total_payload_bytes`** in the JSON is a **counterfactual** total if the same quantizer path were written as **FR2 rev 1**-style tuple-only streams; **FR2 rev 3** adds a one-byte **AC mode tag** per block, so the on-wire size can be slightly **larger** than that counterfactual even when no block picks rANS — that is expected and separate from the **auto** rule (rANS vs explicit **within** rev 3).

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
