# SRSV2 rate control (encoder-side)

This document describes the **first-pass, deterministic** rate-control hook used by `libsrs_video` and exercised by `bench_srsv2`. It is **not** a full multi-pass encoder product feature yet.

## Modes (`SrsV2RateControlMode`)

| Mode | Meaning |
|------|---------|
| **FixedQp** | Every frame uses `SrsV2EncodeSettings::quantizer`, clamped to `[min_qp, max_qp]`. CLI: `--rc fixed-qp` (default) with `--qp`. |
| **ConstantQuality** | Treats `settings.quality` as the **QP index** (51-step quantizer space). **Lower `quality` ⇒ lower QP ⇒ higher quality** (CRF-like naming; the value maps directly to QP after clamp). CLI: `--rc quality --quality N`. |
| **TargetBitrate** | After each encoded frame, compares payload size to a per-frame byte budget derived from `target_bitrate_kbps` and FPS; raises QP if the frame was too large, lowers QP if too small, respecting `qp_step_limit_per_frame` and min/max QP. I-frames use a simple **3×** larger byte allowance vs P-frames. CLI: `--rc target-bitrate --target-bitrate-kbps N` plus `--qp` as the starting QP. |

Optional **`max_bitrate_kbps`** is stored on settings for future tightening; the first-pass controller does not enforce a hard max yet.

## Defaults (`SrsV2EncodeSettings`)

- `rate_control_mode`: FixedQp  
- `quantizer`: 24 (codec default; benchmarks often pass `--qp`)  
- `min_qp`: 4, `max_qp`: 51, `qp_step_limit_per_frame`: 2  

Validation rejects `min_qp > max_qp`, zero target bitrate in target mode, and missing `quality` in constant-quality mode.

## Benchmark output

`bench_srsv2` JSON/Markdown includes an `rc` object on successful SRSV2 rows when details are emitted: mode string (`fixed-qp` / `quality` / `target-bitrate`), target vs achieved bitrate, bitrate error percent, min/max/average QP, per-frame QP list, QP count summary, per-frame payload sizes, and a short byte histogram summary.

## Related docs

- `docs/srsv2_benchmarks.md` — measurement harness flags (`--compare-residual-modes`, `--sweep`, `--rc`, etc.).  
- `docs/srsv2_codec.md` — codec scope and security notes.
