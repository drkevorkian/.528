# SRSV2 motion search (integer-pel, experimental)

P-frame motion estimation is **integer-pel only** (no sub-pel). `SrsV2MotionSearchMode` selects the search strategy within `SrsV2EncodeSettings::motion_search_radius` (clamped for hostile-input safety).

Implemented modes include **None**, **Diamond**, **Hex**, **Hierarchical**, **ExhaustiveSmall** (full window). **`early_exit_sad_threshold`** may terminate search early when best SAD is already at or below the threshold. **`enable_skip_blocks`**: when **true**, a Y **8×8** sub-block may be skipped when residual max-abs is below an encoder threshold; when **false**, **never** emit skip bits — every non-skipped path carries quantized residuals (benchmark **`skip_subblocks_total`** must be zero).

Skip and motion syntax follow **`FR2` revision 2 / 4** (`docs/video_bitstream_v2.md`). Malformed motion or pattern bytes must fail decode safely (decoder limits).

**Sub-pel**, **B-frames**, and **GPU** motion are not implemented. Use `bench_srsv2` flags `--motion-search`, `--early-exit-sad-threshold`, and `--enable-skip-blocks true|false` for measurements; do not assume one mode is universally faster or better without benchmarking your content.

When an experimental **loop filter** is enabled (`docs/deblock_filter.md`), motion-compensated prediction uses the **filtered** reference—the encoder refreshes its reference with the same decode step as the decoder, so MV choices stay consistent with reconstruction.
