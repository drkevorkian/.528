# SRSV2 adaptive quantization (experimental)

This workspace implements **frame-level** adaptive quantization for SRSV2 encode paths: the encoder analyzes **16×16** luma macroblock activity (variance, edge strength, gradients; optional screen-oriented scoring) and derives **one effective QP per frame**, clamped to `[min_qp, max_qp]`. The on-wire frame header carries that **`base_qp`** byte for revisions **1**–**6** and **7**–**9** (rev **7**–**9** also carry **`clip_min` / `clip_max`** bytes after `base_qp`; see `docs/video_bitstream_v2.md`).

**Optional block-level AQ** (`SrsV2BlockAqMode::BlockDelta`, experimental): when combined with **adaptive residual entropy** (`auto` / `rans`), encoders emit **versioned** payloads **`FR2\x07`** (intra), **`FR2\x08`** (integer-MV P), **`FR2\x09`** (half-pel P). Each **8×8** residual block carries a signed **`qp_delta`** byte on the wire (decoder-hard bounds **−24..24**; encoders clamp via `min_block_qp_delta` / `max_block_qp_delta`). Effective quantizer per block is **`clamp(base_qp + qp_delta, clip_min, clip_max)`**, then forced **`≥ 1`**. **Rev 1**–**6** streams are unchanged and must decode as before.

**Chroma vs luma:** **Intra rev 7** writes **`qp_delta` per 8×8 block on Y, U, and V** (each plane’s sample variance drives its own deltas). **P-frame rev 8/9** writes **`qp_delta` only for non-skipped luma 8×8 residuals**; chroma is still **reference MV copy** with **no residual**, so there is **no** chroma block QP delta on P in this slice.

**Semantics:** Frame-level AQ compares **16×16** MB activity to the frame median and tends to assign **higher QP** to **busier** macroblocks. Block-level AQ compares **8×8** variance to the **plane** median and tends to assign **lower QP** (negative delta) to **higher-variance** tiles—see `crates/libsrs_video/src/srsv2/block_aq.rs`. Tune **`aq_strength`** and delta clamps; **benchmark before claiming improvement.**

Statistics: frame-level MB hints remain on `SrsV2AqEncodeStats`; **on-wire** block-AQ aggregates are in `SrsV2AqEncodeStats::block_wire` (`SrsV2BlockAqWireStats`). `bench_srsv2` JSON nests **`frame_aq`** (MB activity / effective QP) beside **`block_aq_wire`** (rev 7–9 bytes).

Modes (`SrsV2AdaptiveQuantizationMode`): **Off**, **Activity**, **EdgeAware**, **ScreenAware**. Strength and delta bounds live on `SrsV2EncodeSettings` (`aq_strength`, `min_block_qp_delta`, `max_block_qp_delta`). **Do not treat this as production-grade adaptive quantization.**

See also: `docs/rate_control.md`, `docs/srsv2_benchmarks.md`, `bench_srsv2` flags `--aq`, `--aq-strength`, `--block-aq`, `--block-aq-delta-min`, `--block-aq-delta-max`.
