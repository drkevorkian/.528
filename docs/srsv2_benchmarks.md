# SRSV2 measurement methodology (optional comparisons)

Workspace tools (engineering measurements):

- Generate deterministic YUV420p8 clips (`--out`, `--meta`; patterns include `flat`, **`gradient`** (alias for `gray-ramp`), `moving-square`, `scrolling-bars`, `checker`, `noise`, `scene-cut`):
  - `cargo run -p quality_metrics --bin gen_synthetic_yuv -- --pattern moving-square --width 1920 --height 1080 --frames 60 --fps 60 --seed 528 --out samples/bench/moving_square_1080p.yuv --meta samples/bench/moving_square_1080p.json`
- **Tiny multi-clip corpus** (writes several **64×64** / **128×128** files under one directory; deterministic seeds):
  - `cargo run -p quality_metrics --bin gen_synthetic_yuv -- --preset-corpus tiny --out-dir var/bench/corpus_tiny --seed 528`
- Benchmark SRSV2 core (**no FFmpeg required**; optional x264 via ffmpeg/libx264):
  - `cargo run -p quality_metrics --bin bench_srsv2 -- --input samples/bench/moving_square_1080p.yuv --width 1920 --height 1080 --frames 60 --fps 60 --qp 28 --keyint 30 --motion-radius 16 --residual-entropy auto --report-json var/bench/moving_square_srsv2.json --report-md var/bench/moving_square_srsv2.md`
  - Add `--compare-x264 --x264-crf 23 --x264-preset medium` if `ffmpeg` is on `PATH`. JSON/Markdown reports then include **x264 preset**, **CRF**, **achieved x264 bitrate** (when measurable), **SRSV2 bitrate at compare time**, **PSNR-Y / SSIM-Y for both**, and a **documented FFmpeg command string**. **`--match-x264-bitrate`** **fails fast** (not implemented — use RC sweeps or target bitrate instead).
  - **PSNR-Y JSON note:** when decoded luma matches the source exactly, raw PSNR is infinity; JSON cannot store `inf`, so the bench maps that case to **100.0 dB** as a finite sentinel (“lossless on luma for this measurement”), not a physical ceiling claim.
  - Residual coding: `--residual-entropy auto|explicit|rans`. **`auto`** never chooses rANS for a block when that would be larger than explicit tuples (unless forced **`rans`**). Reports include intra/P **explicit vs rANS** counts and optional **`legacy_explicit_total_payload_bytes`** when not `explicit`.
- **Compare residual modes** (single command, three encode passes — **no FFmpeg**): `--compare-residual-modes` produces rows **SRSV2-explicit**, **SRSV2-auto**, **SRSV2-rans**. If forced **rans** fails (e.g. coefficients outside the static rANS alphabet), that row is marked failed with an error string and the other rows still appear.
- **Rate control** (benchmark loop only; see `docs/rate_control.md`): `--rc fixed-qp|quality|target-bitrate` with `--quality`, `--target-bitrate-kbps`, optional `--max-bitrate-kbps`, and `--min-qp` / `--max-qp` / `--qp-step-limit`. Reports include achieved vs target bitrate and QP history summaries.
- **Adaptive quantization** (experimental — see `docs/adaptive_quantization.md`): `--aq off|activity|edge-aware|screen-aware`, `--aq-strength N`. **Block-level AQ** (experimental): `--block-aq off|frame-only|block-delta` ( **`block-delta`** requires **`auto`/`rans`** residuals), `--block-aq-delta-min` / `--block-aq-delta-max` (encoder clamp; must fit wire **±24** when **`block-delta`**). JSON nests **`frame_aq`** (16×16 MB activity → effective QP) and **`block_aq_wire`** (on-wire 8×8 `qp_delta`, rev **7**–**9**), plus **`block_aq_mode`** and **`fr2_revision_counts`** (includes **rev14** when half-pel **B** or weighted **B** is on).
- **Motion search** (see `docs/motion_search.md`): `--motion-search none|diamond|hex|hierarchical|exhaustive-small`, `--early-exit-sad-threshold N`, `--enable-skip-blocks` optional bool (`true` default; pass `--enable-skip-blocks false` to disable P-frame skip markers — integration tests assert **`skip_subblocks_total == 0`**). **Experimental P half-pel:** `--subpel off|half` (default **`off`**), `--subpel-refinement-radius N` (clamped; **`0`** skips subpel SAD refinement). JSON/Markdown **`motion`** detail includes subpel mode, tested/selected block counts, extra SAD evaluations, average fractional MV magnitude (quarter-pel units per MB), and **`b_motion_search_mode`** for the **FR2** rev **13** / **14** **B** path when **`--bframes 1`**.
- **B-frame motion (`--bframes 1` only):** `--b-motion-search off|reuse-p|independent-forward-backward|independent-forward-backward-half` (default **`off`**). **`reuse-p`** is an alias for **`off`** today. **`independent-forward-backward`** runs integer ME per ref, then picks forward / backward / average by SAD (**`FR2` rev 13**). **`independent-forward-backward-half`** adds half-pel refinement on the **even quarter-pel** grid (**`FR2` rev 14**). **`--b-weighted-prediction`** enables a small fixed **`/256`** weight candidate set per macroblock when compatible (**rev 14**); JSON reports **`b_blend`** counters including **`b_weighted_*`** and **`b_subpel_*`** fields.
- **Compare B modes (single command, no FFmpeg):** `--compare-b-modes` runs **SRSV2-P-only**, **SRSV2-B-int**, **SRSV2-B-half**, and **SRSV2-B-weighted** rows using the same clip/QP/motion/AQ settings; failures surface as **`error`** on that row (no silent downgrade). Combine with **`--compare-x264`** to append an optional x264 row when FFmpeg works.
- **Inter MV/header syntax (experimental, opt-in):** **`--inter-syntax raw|compact|entropy`** selects legacy **FR2** rev **2/4/5/6/8/9** (**P**) / **10–14** (**B**) vs compact **15**/**16** vs entropy **17**/**18** when applicable. **`--compare-inter-syntax`** runs **SRSV2-raw**, **SRSV2-compact**, and **SRSV2-entropy** in one report; a failed variant (e.g. entropy) keeps an **error row** without aborting siblings. JSON **`srsv2`** (and each compare row’s **`details`**) includes **`mv_*`** aggregates (**`mv_prediction_mode`** is **`median-left-top-topright`** when populated), **`inter_header_bytes`**, **`inter_residual_bytes`**, and **`fr2_revision_counts`** for **rev15–18**.
- **Fast RDO (experimental):** **`--rdo off|fast`** and **`--rdo-lambda-scale N`** (fixed-point; **256 ≈ 1.0**). **`--compare-rdo`** emits **SRSV2-rdo-off** vs **SRSV2-rdo-fast**. Reports include **`rdo_*`** counters (candidates tested, per-mode decisions, **`estimated_bits_used_for_decision`**). **Heuristic only** — not production Lagrangian RDO.
- **Sweep grid** (optional regression / weak-spot finder): `--sweep` runs a fixed grid of QP values `{18, 22, 28, 34}` × residual `{explicit, auto}` × motion radius `{0, 8, 16}` and writes a JSON array plus a Markdown table (`--compare-residual-modes` and `--sweep` are mutually exclusive). **`--sweep-extended`** appends optional rows: AQ/motion comparisons, a small **integer vs half-pel** grid (`subpel-*`), and **`blockaq-off` vs `blockaq-delta`** rows — not enabled by default.

Legacy helper: `cargo run -p codec_compare -- --help` (optional **libx264** branch via `ffmpeg`).

This file describes **reproducible** measurement practices when you compare SRSV2 to **other** video encoders (for example a common **AVC** baseline). It is **not** a scorecard and implies **no** ranking — quality trade-offs are for **you** to judge. This is a **compression engineering** step for the native codec, **not** a claim about beating H.264 or any other standard encoder.

### Local sample numbers (moving-square 128×128, 30 frames, seed 528)

These are **one machine’s** sanity snapshots before enabling experimental **B** / **alt-ref** tooling; your totals will differ by OS, CPU, and build profile. Clip: `gen_synthetic_yuv` pattern **moving-square**, **128×128**, **30** frames, **30** fps. Commands used four `bench_srsv2` runs writing JSON/Markdown under `var/bench/`:

| Configuration (residual-entropy / block-aq / subpel) | Total SRSV2 payload bytes (approx.) | PSNR-Y (approx.) | SSIM-Y (approx.) |
|-----------------------------------------------------|--------------------------------------|------------------|------------------|
| auto / off / off | ~16.7 KiB | mid-26 dB | ~0.988 |
| auto / block-delta / off | ~17.6 KiB | mid-26 dB | ~0.988 |
| auto / off / half | ~24.3 KiB | mid-26 dB | ~0.989 |
| auto / block-delta / half | ~25.2 KiB | mid-26 dB | ~0.989 |

Optional **`bench_srsv2`** flags (**defaults unchanged**): **`--bframes 0`** keeps the historical **I/P** bench loop. **`--bframes 1`** is **experimental**: **keyint**-aware **I/B/P** placement, **encode order = decode order** (anchors before sandwiched **B** pictures), **`FR2` rev **13** or **14** for **B** depending on **`--b-motion-search`** / **`--b-weighted-prediction`**, **`SrsV2ReferenceManager`** throughout (**requires** **`--reference-frames ≥ 2`**, **`--frames ≥ 3`**, 16-aligned size; **not** combinable with **`--sweep`** or **`--compare-residual-modes`**). **`--bframes > 1`** fails fast (“only **0** or **1**”). Reports include **`decode_order_frame_indices`**, **`display_order_frame_indices`**, **`avg_p_anchor_bytes`**, **`b_blend`** counters, **`fr2_revision_counts`** (**rev13** / **rev14** when used), and a **Frame-kind payloads** section; overall PSNR/SSIM use **display** (`frame_index`) order. **`--alt-ref on`** errors with **`alt-ref benchmark encode is not wired yet`** (stay honest — **rev 12** decode remains elsewhere). **`--gop N`** (reserved), **`--reference-frames N`** (sequence `max_ref_frames`, default **1**).

**Local engineering baseline (128×128 moving-square, 30 frames, seed 528, QP 28, keyint 30):** generate `var/bench/moving_square_128.yuv` / `.json` as in **TASK 1** commands in the mission brief, then capture JSON/Markdown under `var/bench/`. One **Windows x86_64** lab snapshot (debug `bench_srsv2`):

| Row | Total SRSV2 payload bytes | PSNR-Y (display order) | SSIM-Y | Notes |
|-----|---------------------------|------------------------|--------|--------|
| P-only | 25542 | 25.45 | 0.9864 | Better compression than B modes here; **no** B frames. |
| B-int | 22150 | 24.24 | 0.9887 | Smaller bitstream than P-only on this clip; **lower** PSNR-Y vs P-only at these settings. |
| B-half | 29622 | 24.27 | 0.9882 | Rev **14** wire; MV side info dominates — **larger** than B-int; fractional B MV stats non-zero. |
| B-weighted (optional) | 29318 | 24.24 | 0.9887 | Weight candidates exercised (**`b_weighted_candidates_tested` > 0**) but **no** MB picked weighted vs forward/back/avg on this clip. |

**Local engineering baseline (not marketing proof):** numbers vary by CPU, OS, and build profile; re-run benches rather than treating this table as a product scorecard.

### Example: AQ + motion + skip flags (128×128 moving-square)

After generating `var/bench/moving_square.yuv` (or any raw YUV420p8 clip):

```bash
cargo run -p quality_metrics --bin bench_srsv2 -- \
  --input var/bench/moving_square.yuv \
  --width 128 --height 128 --frames 30 --fps 30 \
  --qp 28 --keyint 30 \
  --motion-search diamond \
  --enable-skip-blocks true \
  --aq activity \
  --residual-entropy auto \
  --report-json var/bench/moving_square_aq_motion.json \
  --report-md var/bench/moving_square_aq_motion.md
```

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
   - **Bitrate** matched either by **two-pass** targeting bitrate or by **CRF** with **reported achieved bitrate for both sides**. **CRF-only** labels without achieved bitrate and quality numbers are **not** sufficient for serious encoder comparisons — prefer **`bench_srsv2`** x264 rows when FFmpeg is available, or manual sweeps (`docs/h264_competition_plan.md`).
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
