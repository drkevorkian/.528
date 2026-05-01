# SRSV2 codec overview

**SRSV2** (`codec_id` **3** in `.528`, elementary `.srsv2`) is the **default** video codec for **new** `.528` files created by encode, import, transcode, and mux workflows in this workspace (unless callers explicitly select legacy SRSV1). Long-term intent is an **8K-first**, scalable native codec — see **`docs/srsv2_design_targets.md`** for profiles, presets policy, and roadmap. **`docs/srsv2_benchmarks.md`** is an **optional** guide for reproducible measurements if you compare SRSV2 to other encoders yourself; the project does not rank codecs or publish superiority claims.

**SRSV1** (`codec_id` **1** in `.528`, elementary `.srsv`) is **legacy / prototype** compatibility: grayscale intra, still fully readable and writable for tests and older assets.

**SRSV2** is a native bitstream (not MPEG): block prediction, transforms, quantization, and framed residuals. It does **not** interoperate with H.264/HEVC/AV1/VVC elementary streams and does not embed third-party codec sources.

**Default CLI `encode` to `.528`** (square raw → `.srsv2` elementary) is still a **single intra** frame (`FR2\x01`, `max_ref_frames` 0 in the default sequence helper). **`SrsV2EncodeSettings::residual_entropy`** selects intra wire format: **`explicit`** keeps **`FR2\x01`** (tuple-only blocks); **`auto`** / **`rans`** emit **`FR2\x03`** with per-block **explicit vs static rANS** AC packing when enabled (see `docs/video_bitstream_v2.md`). **Native import** (SRSV2 policy) writes **`max_ref_frames = 1`** and emits **P** frames: legacy **`FR2\x02`** when residuals are tuple-only, or **`FR2\x04`** when adaptive residual modes are active. **P-frame status:** **16×16 integer-pel** motion, bounded MV search (`SrsV2EncodeSettings::motion_search_radius`), skip/residual **Y** blocks, chroma from reference with half MVs; import refreshes the encoder reference with **`decode_yuv420_srsv2_payload`** so the chain matches playback.

**Rate control:** `SrsV2EncodeSettings` includes **`rate_control_mode`** (**fixed QP**, **constant-quality**, **target bitrate**), QP bounds, and a **`SrsV2RateController`** used by benchmark tooling (`bench_srsv2`) for deterministic per-frame QP selection. Details and CLI mapping: **`docs/rate_control.md`**. This is a **first-pass** controller for measurements and encoder-side QP selection — **not** a completed broadcast-grade RC loop.

**Adaptive quantization (experimental):** optional frame-level QP derivation from per-MB activity (`docs/adaptive_quantization.md`). The bitstream still carries **one QP byte** per frame; there is no per-block QP delta syntax yet.

**Motion search (experimental):** integer-pel modes and skip thresholds (`docs/motion_search.md`); still **no sub-pel**.

Sub-pel/B-frames, richer closed-loop RC, GPU codecs, and OS audio/video output remain **future slices**.

## Implemented in this repository

- 64-byte `SRS2` sequence header (little-endian fields + profile/pixel/color metadata), including **`max_ref_frames`** (capped; enables reference pictures for **P** prototype).
- YUV420p8 intra frame payloads: **`FR2\x01`** (explicit coefficient tuples only) and experimental **`FR2\x03`** (adaptive explicit vs static rANS per **8×8** block).
- Experimental P-frame payloads: **`FR2\x02`** (tuple residuals) and **`FR2\x04`** (adaptive residuals); integer MV per 16×16 MB (`libsrs_video::srsv2::p_frame_codec`).
- Elementary `.srsv2` streams (sync + CRC-framed payloads).
- Container mux/demux with `codec_id == 3` and bounded playback decode for primary video (`decode_yuv420_srsv2_payload`).
- CLI: `encode --codec srsv2`, `analyze --dump-codec`, decode of `.srsv2` to raw YUV via app services.

## Planned / not yet merged

- Half-pel motion, **B**-frames, and richer merged MV modes beyond the integer-pel **P** prototype.
- Broader entropy coding (per-file trained models, MV syntax, etc.). Today: **experimental** static rANS **AC residual** tokens only; motion and headers remain structured bytes with bounds checks.
- Loop filters beyond optional stubs, HDR signaling beyond sequence fields, 10-bit encode/decode paths.
- GPU backends (`gpu-wgpu`, `gpu-cuda` feature placeholders).

## Security model

Decoders treat all inputs as hostile: capped dimensions, capped payloads, no panics on malformed syntax, structured `SrsV2Error` / container errors. See `docs/video_bitstream_v2.md` for limits.
