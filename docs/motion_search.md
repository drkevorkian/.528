# SRSV2 motion search (experimental)

## Integer-pel search

`SrsV2MotionSearchMode` selects the **integer-pel** strategy within `SrsV2EncodeSettings::motion_search_radius` (clamped for hostile-input safety). Implemented modes include **None**, **Diamond**, **Hex**, **Hierarchical**, **ExhaustiveSmall** (full window). **`early_exit_sad_threshold`** may terminate search early when best SAD is already at or below the threshold. **`enable_skip_blocks`**: when **true**, a Y **8×8** sub-block may be skipped when residual max-abs is below an encoder threshold; when **false**, never emit skip bits — every non-skipped path carries quantized residuals (benchmark **`skip_subblocks_total`** must be zero).

Legacy predicted payloads **`FR2` revision 2 / 4** carry **`i16`** motion vectors in **full-pixel** units only.

## Half-pel (experimental, opt-in)

When **`SrsV2EncodeSettings::subpel_mode == HalfPel`**, the encoder keeps the same integer search, then optionally refines each macroblock on an **eight-offset half-pel ring** in **quarter-pel units** (`±2` == half-pel on the ¼ grid). Wire format: **`FR2\x05`** (tuple residuals, same layout as rev **2** after MV width) or **`FR2\x06`** (adaptive residuals like rev **4**). MVs are stored as **`i32` LE** in **quarter-pel** luma units; decoders reject **odd** quarter values (not on the half-pel grid).

**`SrsV2EncodeSettings::subpel_refinement_radius`** is clamped ( **`0`** skips refinement; **`1`** runs the default eight probes). **Quarter-pel** motion beyond the current half-pel grid is not implemented for **P** or **B**.

**Experimental B** syntax (**`FR2\x0A`** … **`FR2\x0E`**) decodes in **`libsrs_video`** and **`PlaybackSession`** when **`max_ref_frames ≥ 2`** (typically **decode-order** *I₀→P₂→B₁*). **`bench_srsv2`** supports **`--bframes 1`** as an **experimental** GOP benchmark. **`SrsV2BMotionSearchMode::IndependentForwardBackward`** drives **`FR2` rev 13** (integer MV per MB). **`IndependentForwardBackwardHalfPel`** runs integer ME first, then half-pel probes on the quarter grid (**even qpel only**) around the best forward/backward candidates and emits **`FR2` rev 14** when selected.

**GPU** motion search is not implemented.

**Chroma** still uses an **integer** copy with **`mv_q / 8`** (approximation); full chroma sub-pel is future work — benchmark before claiming chroma-aware gains.

Use `bench_srsv2` **`--subpel off|half`** and **`--subpel-refinement-radius N`** for measurements. **Default remains integer-only (`Off`).** Do not assume half-pel improves bitrate or objective scores without measuring your content.

When an experimental **loop filter** is enabled (`docs/deblock_filter.md`), prediction uses the **filtered** reference — the encoder refreshes its reference with the same **`decode_yuv420_srsv2_payload`** step as playback, so MV choices stay consistent with reconstruction.
