# SRSV2 motion search (integer-pel, experimental)

P-frame motion estimation is **integer-pel only** (no sub-pel). `SrsV2MotionSearchMode` selects the search strategy within `SrsV2EncodeSettings::motion_search_radius` (clamped for hostile-input safety).

Implemented modes include **None**, **Diamond**, **Hex**, **Hierarchical**, **ExhaustiveSmall** (full window). **`early_exit_sad_threshold`** may terminate search early when best SAD is already at or below the threshold. **`enable_skip_blocks`** gates skip detection for low-residual macroblocks.

Skip and motion syntax follow **`FR2` revision 2 / 4** (`docs/video_bitstream_v2.md`). Malformed motion or pattern bytes must fail decode safely (decoder limits).

**Sub-pel**, **B-frames**, and **GPU** motion are not implemented. Use `bench_srsv2` flags `--motion-search`, `--early-exit-sad-threshold`, and `--enable-skip-blocks` for measurements; do not assume one mode is universally faster or better without benchmarking your content.
