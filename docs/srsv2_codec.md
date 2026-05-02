# SRSV2 codec overview

**SRSV2** (`codec_id` **3** in `.528`, elementary `.srsv2`) is the **default** video codec for **new** `.528` files created by encode, import, transcode, and mux workflows in this workspace (unless callers explicitly select legacy SRSV1). Long-term intent is an **8K-first**, scalable native codec — see **`docs/srsv2_design_targets.md`** for profiles, presets policy, and roadmap. **`docs/srsv2_benchmarks.md`** is an **optional** guide for reproducible measurements if you compare SRSV2 to other encoders yourself; the project does not rank codecs or publish superiority claims.

**SRSV1** (`codec_id` **1** in `.528`, elementary `.srsv`) is **legacy / prototype** compatibility: grayscale intra, still fully readable and writable for tests and older assets.

**SRSV2** is a native bitstream (not MPEG): block prediction, transforms, quantization, and framed residuals. It does **not** interoperate with H.264/HEVC/AV1/VVC elementary streams and does not embed third-party codec sources.

**Default CLI `encode` to `.528`** (square raw → `.srsv2` elementary) is still a **single intra** frame (`FR2\x01`, `max_ref_frames` 0 in the default sequence helper). **`SrsV2EncodeSettings::residual_entropy`** selects intra wire format: **`explicit`** keeps **`FR2\x01`** (tuple-only blocks); **`auto`** / **`rans`** emit **`FR2\x03`** with per-block **explicit vs static rANS** AC packing when enabled, or **`FR2\x07`** when **`SrsV2BlockAqMode::BlockDelta`** is enabled (same entropy path plus per-block **`qp_delta`**). **Native import** (SRSV2 policy) writes **`max_ref_frames = 1`** and emits **P** frames: legacy **`FR2\x02`** when residuals are tuple-only and **`SrsV2SubpelMode::Off`**, or **`FR2\x04`** when adaptive residual modes are active (still integer MV); optional experimental **`FR2\x05` / `FR2\x06`** when **`SrsV2SubpelMode::HalfPel`** is enabled (quarter-pel–grid MVs, **even** quarter-pel units only); with **`BlockDelta`**, payloads upgrade to **`FR2\x08`** (integer MV) or **`FR2\x09`** (half-pel). **Half-pel luma motion** is **experimental** and **opt-in** via **`subpel_mode`** / bench **`--subpel half`**. **MVs** are stored in **quarter-pel units** with **half-pel steps = ±2** (odd quarter-pel values are malformed). **Chroma** still uses a **limited integer** approximation (**`mv/2`** or **`mv_q/8`**). **P-frame status:** **16×16** macroblocks, bounded MV search (`motion_search_radius`), skip/residual **Y** blocks; import refreshes the encoder reference with **`decode_yuv420_srsv2_payload`** so the chain matches playback.

**Rate control:** `SrsV2EncodeSettings` includes **`rate_control_mode`** (**fixed QP**, **constant-quality**, **target bitrate**), QP bounds, and a **`SrsV2RateController`** used by benchmark tooling (`bench_srsv2`) for deterministic per-frame QP selection. Details and CLI mapping: **`docs/rate_control.md`**. This is a **first-pass** controller for measurements and encoder-side QP selection — **not** a completed broadcast-grade RC loop.

**Adaptive quantization (experimental):** optional **frame-level** QP derivation from per-MB activity (`docs/adaptive_quantization.md`). Optional **block-level** **`qp_delta`** syntax is **versioned** (**`FR2\x07`–`\x09`**) and **off by default** (`SrsV2BlockAqMode`). Frame-level AQ still picks the **`base_qp`** byte before per-block deltas apply. **Intra rev 7** carries **`qp_delta` on Y/U/V 8×8 blocks**; **P rev 8/9** carries **`qp_delta` on luma residuals only** (chroma has no residual syntax in this prototype).

**Motion search (experimental):** integer-pel modes, optional **half-pel** refinement (`docs/motion_search.md`). **Experimental B** (`FR2\x0A`/`\x0B`) and **alt-ref** (`FR2\x0C`) decode through **`decode_yuv420_srsv2_payload_managed`** / **`SrsV2ReferenceManager`** (parser-safe baseline). **Playback** accepts **B** when **`max_ref_frames ≥ 2`** and packet order matches decode needs (often *I₀ → P₂ → B₁*); **`max_ref_frames < 2`** → **`PlaybackError::Unsupported`**. **`classify_srsv2_payload`** treats rev **10**/**11** like other **non-keyframe** **predicted** kinds for mux/index policy. Quality and tooling are **not** production-grade. Finer **GPU** motion remains roadmap.

### Experimental B frames and alt-ref (baseline semantics)

- **Rev 10 (`FR2\x0A`):** B-frame syntax with **integer** MV grid (parser-safe / minimal baseline).
- **Rev 11 (`FR2\x0B`):** B-frame syntax with **half-pel** MV grid (same experimental tier).
- **Rev 12 (`FR2\x0C`):** **Non-displayable** alt-ref / hidden reference refresh (`is_displayable == false`); updates **`SrsV2ReferenceManager`** only.
- **Current B encoder (when enabled)** is intentionally minimal: **average** blend (`BBlendModeWire::Average`), **zero MV** baseline unless improved later; residual entropy follows the same experimental paths as **P**. **B-frame compression optimization** (bidirectional motion search, weighted prediction, joint RDO B/P/I decisions, tuned benchmark encode presets) remains **future work** — not “B syntax unavailable,” but **not** mature compression yet.

Richer closed-loop RC, GPU codecs, and OS audio/video output remain **future slices**.

## Implemented in this repository

- 64-byte `SRS2` sequence header (little-endian fields + profile/pixel/color metadata), including **`max_ref_frames`** (capped; enables reference pictures for **P** prototype).
- YUV420p8 intra frame payloads: **`FR2\x01`** (explicit coefficient tuples only); experimental **`FR2\x03`** (adaptive explicit vs static rANS per **8×8** block); experimental **`FR2\x07`** (rev **3** block layout + per-block **`qp_delta`**).
- Experimental P-frame payloads: **`FR2\x02`** / **`FR2\x04`** (integer **`i16`** MVs); **`FR2\x05`** / **`FR2\x06`** (half-pel grid, **`i32`** quarter-pel MVs); **`FR2\x08`** / **`FR2\x09`** (rev **4**/**6** residuals + per-chunk **`qp_delta`**).
- Experimental **B** payloads **`FR2\x0A`** (integer MV) / **`FR2\x0B`** (half-pel MV) and **alt-ref** **`FR2\x0C`** (non-display reference refresh), parser-safe and bounded by **`max_ref_frames`**.
- Elementary `.srsv2` streams (sync + CRC-framed payloads).
- Container mux/demux with `codec_id == 3` and bounded playback decode for primary video (`decode_yuv420_srsv2_payload_managed`; legacy **`decode_yuv420_srsv2_payload`** remains for **FR2** rev **1**–**9** single-slot callers).
- CLI: `encode --codec srsv2`, `analyze --dump-codec`, decode of `.srsv2` to raw YUV via app services.

## Planned / not yet merged

- General **quarter-pel** motion beyond the current half-pel grid, **B** half-pel ME, **weighted B** prediction, **B** RDO, and production-grade **GOP** / **B** placement (beyond **`FR2` rev 13** bench measurements and **`bench_srsv2 --bframes 1`** keyint-aware lab GOPs).
- Broader entropy coding (per-file trained models, MV syntax, etc.). Today: **experimental** static rANS **AC residual** tokens only; motion and headers remain structured bytes with bounds checks.
- **Loop filter (experimental):** when `disable_loop_filter` is **false**, encoder and decoder apply the same **simple luma deblock** on reconstructed **Y** before refreshing the SRSV2 reference (see **`docs/deblock_filter.md`**). **CDEF**, **restoration**, **film grain**, and chroma loop filtering are **not** implemented.
- GPU backends (`gpu-wgpu`, `gpu-cuda` feature placeholders).

## Security model

Decoders treat all inputs as hostile: capped dimensions, capped payloads, no panics on malformed syntax, structured `SrsV2Error` / container errors. See `docs/video_bitstream_v2.md` for limits.
