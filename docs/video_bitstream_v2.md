# SRSV2 bitstream and container mapping

**Container policy:** New `.528` **video** tracks should use **`codec_id == 3`** (SRSV2) with the 64-byte sequence header embedded in track config. **`codec_id == 1`** (SRSV1) is legacy; players and tools still open and decode it. **`codec_id == 2`** is **audio** (SRSA), not SRSV2 — see `libsrs_container::codec_ids`. The logical enum **`SrsElementaryVideoCodecId`** (alias `SrsVideoCodecId`) in `libsrs_video` uses the same numeric **values** as video container IDs **1** and **3**, but is **not** the mux `codec_id` field type.

## Sequence header (64 bytes, fixed)

- Magic `SRS2` (4 bytes).
- Schema byte `1`.
- Width / height: `u32` LE each (must satisfy decoder caps in `libsrs_video::srsv2::limits`).
- **Profile** byte (see `SrsVideoProfile` in `libsrs_video::srsv2::model`): **0** Baseline, **1** Main, **2** Pro, **3** Lossless, **4** Screen, **5** Ultra, **6** Research — semantics in **`docs/srsv2_design_targets.md`**.
- Pixel format, color primaries, transfer, matrix, chroma siting, range, **loop-filter disable** flag (`disable_loop_filter`: when **false**, SRSV2 applies an experimental **luma loop filter** after reconstructing Y — see [`docs/deblock_filter.md`](deblock_filter.md)), **`deblock_strength`** byte at offset **25** (**`0`** = codec default strength when the filter is enabled; ignored when `disable_loop_filter` is **true**), max reference frames.

Embedded verbatim in `.528` **video track config** when `codec_id == 3`.

## Frame payload (mux packet bytes)

### Revision 1 — intra (`FR2\x01`)

Prefix `FR2\x01`, `frame_index` LE `u32`, `qp` byte, then three length-prefixed plane bitstreams (Y, U, V) for YUV420p8 intra.

**QP byte (`base_qp`):** encoders may choose this value using rate control and/or **experimental frame-level adaptive quantization**. For revisions **1**–**6**, decoders use this single byte as the frame quantizer (no per-block deltas). **Revision 7**–**9** add optional **per-8×8 `qp_delta`** bytes (see below); effective QP per block clamps **`base_qp + qp_delta`** using **`clip_min` / `clip_max`** carried in the payload header.

### Revision 2 — experimental P (`FR2\x02`)

Prefix `FR2\x02`, `frame_index`, `qp`, then per **16×16** macroblock (coverage requires width/height divisible by 16): `mv_x`, `mv_y` (`i16` LE, bounded by `MAX_MOTION_VECTOR_PELS`), `pattern` byte (four bits mark skip for four **8×8** Y sub-blocks), then optional length-prefixed residual blobs for non-skipped sub-blocks using **legacy explicit** intra-style coefficient tuples per **8×8** block. Chroma U/V are predicted by copying the reference planes with half-resolution MVs (no chroma residual in this slice). Decode requires `max_ref_frames ≥ 1` and a valid reference frame (`PFrameWithoutReference` otherwise).

### Revision 3 — intra with adaptive residual coding (`FR2\x03`) — experimental

Prefix matches **`FR2\x03`**. Same top-level layout as revision **1** (`frame_index`, `qp`, three length-prefixed Y/U/V plane blobs). Within each plane, **8×8** blocks are packed as: prediction **mode** byte, **DC** (`i16` LE), then a **tag** byte selecting **explicit AC tuples** (same syntax as rev **1** plane bodies) or **static rANS**-packed AC token stream (`sym_count` `u16` LE, `byte_len` `u16` LE, bytes). Encoders pick the smaller representation per block when `residual_entropy = auto`; `explicit` forces tuples; `rans` forces entropy where legal. **Rev 1** payloads remain the canonical **explicit-tuple-only** intra format; **rev 3** is an optional compression improvement path.

### Revision 4 — P-frame with adaptive residuals (`FR2\x04`) — experimental

Same macroblock grid and motion syntax as revision **2**. Non-skipped **8×8** Y residuals use either legacy tuple blobs or an **adaptive** layout byte **`1`** followed by the same per-block explicit vs rANS tagging as intra **rev 3**. **Rev 2** remains the tuple-only P payload for backward compatibility.

### Revision 5 — P-frame with half-pel luma MVs (`FR2\x05`) — experimental

Same macroblock grid and residual blob layout as revision **2**, except each macroblock carries **`mv_x_q`, `mv_y_q` as `i32` LE** in **quarter-pel** luma units (half-pel steps are **`±2`**). Values must lie within decoder MV caps and be **even** in quarter-pel space (odd values are **malformed**). Chroma prediction uses integer **`mv_q / 8`** (limited approximation).

### Revision 6 — P-frame half-pel MVs + adaptive residuals (`FR2\x06`) — experimental

Same as revision **5** for motion, with non-skipped residual packing matching revision **4** (adaptive explicit vs rANS).

### Revision 7 — intra + adaptive residuals + block `qp_delta` (`FR2\x07`) — experimental

Prefix **`FR2\x07`**. After `frame_index` (`u32` LE) and **`base_qp`** (`u8`), **`clip_min`** and **`clip_max`** (`u8` each, inclusive QP clip range; decoders reject `clip_min == 0`, `clip_min > clip_max`, or `clip_max > 51`). Three length-prefixed Y/U/V planes follow. Within **each** plane, each **8×8** block is: prediction **mode**, **DC** (`i16` LE), signed **`qp_delta`** (`i8`, wire range **−24..24**), then the same **tag** + AC payload as revision **3**. U and V use **independent** variance-driven deltas per chroma **8×8** block (not tied to collocated luma `qp_delta`). Decoders validate **`qp_delta`** and compute effective QP per block before dequantization.

### Revision 8 — P-frame integer MV + adaptive residuals + block `qp_delta` (`FR2\x08`) — experimental

Same macroblock grid and motion syntax as revision **4**, with the same **`clip_min` / `clip_max`** bytes after **`qp`** as revision **7**. For each non-skipped **luma** **8×8** residual chunk: **`qp_delta`** (`i8`) precedes the **`u32` LE chunk length** and chunk bytes (layout **`0`** legacy tuple or **`1`** adaptive, matching rev **4**). Skipped sub-blocks omit **`qp_delta`** and chunk payload. Chroma remains **reference copy** only (no chroma residual, hence **no** chroma **`qp_delta`**).

### Revision 9 — P-frame half-pel + adaptive residuals + block `qp_delta` (`FR2\x09`) — experimental

Same as revision **8** for **`qp_delta`** placement, clipping header, and **luma-only** residual deltas (chroma MV-copy only), with motion syntax matching revision **6** (**`i32` LE** quarter-pel MVs, even quarter-pel grid).

### Revision 15 — experimental **P** compact inter MV (`FR2\x0F`) — **opt-in**

**Experimental:** median-predicted MV deltas as **zigzag signed varints** (left / top / top-right median per quarter-pel component stream), then the same **rev 8**/**rev 9**-style residual bodies (skip pattern + adaptive chunks + optional block AQ). **Default encoder output remains raw legacy rev 2/4/5/6/8/9** unless settings explicitly select compact inter syntax.

### Revision 17 — experimental **P** entropy-coded inter MV (`FR2\x11`) — **opt-in**

Same compact MV **symbol bytes** as rev **15**, plus a **bounded** static **rANS** blob over those bytes (**sym_count**, **blob_len**, blob) before residuals. **Not** CABAC-class; **not** context adaptive.

### Revision 10 — experimental B-frame, integer MV (`FR2\x0A`)

B-frame **macroblock** syntax (16-aligned canvas; mux/policy may still classify **`FR2\x0A`/`\x0B`** as generic **predicted** / non-keyframe): `frame_index`, `qp`, **`slot_a`**, **`slot_b`**, blend mode, per-MB MV pair(s) (`i16` when rev **10**), adaptive residual packing akin to **P** rev **4**. Requires **`max_ref_frames ≥ 2`**, valid populated slots, backward reference strictly **before** the current picture in `frame_index` order and forward reference strictly **after**, and a supported blend (**weighted** on wire value **3** is reserved / rejected). Parser rejects malformed residuals, bad MVs, and oversize payloads.

### Revision 11 — experimental B-frame, half-pel MV (`FR2\x0B`)

Same as revision **10** but MVs are **`i32` LE** quarter-pel (even grid), matching **P** rev **6** motion packing.

### Revision 13 — experimental B-frame, per-MB blend + integer MV (`FR2\x0D`)

Integer MV only (`i16` LE per reference, four components per macroblock: backward MV then forward MV). After `frame_index`, `qp`, `slot_a`, `slot_b` there is **no** frame-level blend byte: each macroblock begins with **`blend`** (`u8`, same semantics as rev **10**: forward **0**, backward **1**, average **2**, weighted **3** reserved / rejected), then the four MV components, then the usual **P**-style **8×8** skip pattern and adaptive residual chunks. Encoder chooses **`blend`** per MB by min-SAD among forward / backward / average predictions. **`Weighted`** (**3**) remains **rejected** on decode for rev **13** (use rev **14**).

### Revision 14 — experimental B-frame, per-MB blend + half-pel MV grid + optional weighted blend (`FR2\x0E`)

Same macroblock coverage and slot rules as rev **13**, but motion uses **`i32` LE** quarter-pel components (**backward** then **forward**, four values per MB). Only **even** quarter-pel steps are legal (**half-pel** grid); **odd** quarter values are malformed. MV magnitude is bounded to the same radius family as **P** half-pel revisions (decoder rejects out-of-range vectors).

Per macroblock, after **`blend`**:

- **`blend` ∈ {0,1,2}`**: four **`i32`** MV components, then skip pattern + residuals (same style as rev **13**).
- **`blend == 3` (weighted):** **`weight_a`**, **`weight_b`** (`u8` each). Valid pairs satisfy **`weight_a + weight_b == 256`** with both non-zero; prediction uses integer **`(a * weight_a + b * weight_b + 128) / 256`** with **`clamp(0, 255)`**. Then four **`i32`** MV components, skip pattern, residuals.

**Chroma** MC remains the same **integer approximation** as other SRSV2 inter paths (**`mv_q / 8`** rounding); only **luma** uses the bilinear half-pel sampler.

### Revision 16 — experimental **B** compact inter (`FR2\x10`) — **opt-in**

After `frame_index`, `qp`, `slot_a`, `slot_b`, **`flags`** (`u8`: bit **0** half-pel MV grid, bit **1** weighted blend allowed): **two** back-to-back compact MV grids (**backward** then **forward**, same median+varint syntax as **P** rev **15**), then per-MB blend / weights / residuals using the same compact residual packing as legacy **B** rev **13**/ **14** (without embedding raw MV tuples per MB).

### Revision 18 — experimental **B** entropy inter (`FR2\x12`) — **opt-in**

Same header and dual grids as rev **16**, but each grid’s compact byte sequence is wrapped as **sym_count**, **blob_len**, **rANS blob** (two sections), then per-MB residuals. **Static** rANS model only; bounded decode.

### Revision 19 — experimental **P** variable partition + compact MV (`FR2\x13`) — **opt-in**

After `frame_index`, `qp`, **`flags`** (same low three bits as **P** rev **15**: subpel, block AQ, entropy residuals), optional **`clip_min`/`clip_max`**, **`n_mb`** partition-type bytes (**2** bits **MB type**: **0** = 16×16, **1** = 16×8, **2** = 8×16, **3** = 8×8; reserved high bits rejected), compact partitioned MV byte stream (median prediction), then per **8×8** luma region **ctrl** (**skip**, **transform**: **8×8** vs **4×4** vs reserved **16×16** marker) and length-prefixed residual chunks compatible with **`decode_p_residual_chunk`** / **`decode_p_residual_chunk_4x4`**. **Maximum partition units per frame** = **`macroblocks × 4`** (decoder-enforced). Chroma follows **first PU MV** per macroblock (same approximation family as other **P** revisions).

### Revision 20 — experimental **P** variable partition + entropy MV (`FR2\x14`) — **opt-in**

Same as rev **19**, but the compact MV bytes are wrapped **`sym_count`**, **`blob_len`**, static **rANS** blob (bounded).

### Revision 21 — experimental **B** variable partition + compact inter (`FR2\x15`) — **parser placeholder**

Magic is reserved and classified as **bidirectional** for mux policy; **decode returns structured `Unsupported` in this slice** (no silent pretend-decode).

### Revision 22 — experimental **B** variable partition + entropy inter (`FR2\x16`) — **parser placeholder**

Same honesty rule as rev **21**.

### Revision 12 — experimental alt-ref / hidden reference (`FR2\x0C`)

Non-displayable intra-coded planes (same entropy style as **rev 3** in this slice): `frame_index`, `qp`, **`target_slot`**, **`reserved`** (must be **0**). Picture updates **`SrsV2ReferenceManager`** at **`target_slot`** with **`is_displayable == false`**; playback must **not** treat it as a presented frame.

**Compatibility:** Revisions **1**–**14** remain readable; **15**–**22** extend **opt-in** inter experiments (**15**–**18**: fixed-MB compact/entropy MV; **19**–**20**: **P** variable partitions; **21**–**22**: **B** variable partitions — **not implemented**, honest **`Unsupported`**). The legacy single-slot helper **`decode_yuv420_srsv2_payload`** returns **`Unsupported`** for **10**–**18** and **B**-class **21**/**22** — use **`decode_yuv420_srsv2_payload_managed`** for **B** and managed reference paths.

## Elementary `.srsv2` file

Starts with the 64-byte sequence header, then repeating framed records: VP packet sync (`PACKET_SYNC` from `libsrs_video`), version/type bytes, `frame_index`, payload length, CRC32 of header fields + payload, payload bytes.

## Decoder requirements

- Ignore reserved trailing bytes in the 64-byte sequence header for schema **1** (decoders read defined offsets only); encoders should zero-fill unused slots.
- Reject unknown sequence schema version.
- Enforce `MAX_FRAME_PAYLOAD_BYTES`, dimension caps, and CRC mismatches as hard errors.
- **FR2** revisions **1**–**4** remain the integer-MV baseline; **5** and **6** add **half-pel** luma MVs (experimental). **3** and **4** add optional entropy-coded intra/P residuals; **7**–**9** add optional **block `qp_delta`** with adaptive residuals (see `docs/srsv2_codec.md`).
