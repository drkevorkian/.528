# SRSV2 experimental loop filter (simple luma deblock)

This document describes the **optional**, **experimental** post-reconstruction filter applied to **luma (Y) only** in SRSV2. It is **CPU-only**, **deterministic**, and intentionally minimal — **not** an H.264/AV1-style deblocking loop with BS indices, **not** **CDEF**, **not** **restoration**, and **not** **film grain**.

## Semantics

- **Sequence flag:** `VideoSequenceHeaderV2::disable_loop_filter` (**true** = filter **off**, default; **false** = filter **on**).
- **Strength byte:** offset **25** in the 64-byte sequence header (`deblock_strength`). When the filter is **off**, encoders should write **0**. When the filter is **on**, **`0`** selects the codec-defined default (`libsrs_video::srsv2::deblock::DEFAULT_DEBLOCK_STRENGTH`); **`1…255`** scales smoothing.
- **Where it runs:** After reconstructing the **Y** plane for an intra or **P** frame, **before** exposing the frame and **before** storing it as the next reference for **P** prediction.
- **Encoder / decoder match:** Both sides must call the same `apply_loop_filter_y` routine (`libsrs_video::srsv2::deblock`) with the same `SrsV2LoopFilterMode` and resolved strength derived from the sequence header. Reference refresh in tooling uses `decode_yuv420_srsv2_payload`, which applies the filter when signaled — keeping encode and decode chains aligned.

## Algorithm (high level)

- **`SrsV2LoopFilterMode::Off`:** no-op; output equals reconstruction.
- **`SrsV2LoopFilterMode::SimpleDeblock`:** weak symmetric blend across **8-pixel** vertical and horizontal boundaries (covers **8×8** leaf edges and **16×16** macroblock edges). Pairs are unchanged when the step across the boundary exceeds an edge-preservation threshold derived from strength — strong discontinuities are kept sharp.

**Chroma (U/V)** is never filtered in this slice.

## Benchmarking and metrics

`bench_srsv2` accepts `--loop-filter off|simple` and `--deblock-strength N`. When `--loop-filter simple`, the JSON/Markdown report includes a **deblock** section with primary PSNR-Y / SSIM-Y plus an optional **respin** encode/decode using `disable_loop_filter=true` for comparison. Because the respin uses a **different** filtered reference chain, **payload sizes and objective scores are not directly comparable** as “the same encode with a toggle”; interpret differences cautiously. **Deblocking may reduce PSNR-Y vs source while improving subjective blocking artifacts.**

## Future work

Modern codecs typically add **in-loop** filters beyond simple boundary smoothing (SAO, ALF, CDEF, film-grain synthesis, etc.). None of that is committed here; any extension must be **signaled**, **specified**, and **matched** on encode and decode.
