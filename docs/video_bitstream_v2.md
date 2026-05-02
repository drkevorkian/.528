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

### Revision 10 — experimental B-frame, integer MV (`FR2\x0A`)

Bidirectional **macroblock** syntax (16-aligned canvas): `frame_index`, `qp`, **`slot_a`**, **`slot_b`**, blend mode, per-MB MV pair(s) (`i16` when rev **10**), adaptive residual packing akin to **P** rev **4**. Requires **`max_ref_frames ≥ 2`**, valid populated slots, backward reference strictly **before** the current picture in `frame_index` order and forward reference strictly **after**, and a supported blend (**weighted** on wire value **3** is reserved / rejected). Parser rejects malformed residuals, bad MVs, and oversize payloads.

### Revision 11 — experimental B-frame, half-pel MV (`FR2\x0B`)

Same as revision **10** but MVs are **`i32` LE** quarter-pel (even grid), matching **P** rev **6** motion packing.

### Revision 12 — experimental alt-ref / hidden reference (`FR2\x0C`)

Non-displayable intra-coded planes (same entropy style as **rev 3** in this slice): `frame_index`, `qp`, **`target_slot`**, **`reserved`** (must be **0**). Picture updates **`SrsV2ReferenceManager`** at **`target_slot`** with **`is_displayable == false`**; playback must **not** treat it as a presented frame.

**Compatibility:** Revisions **1**–**9** remain the stable interchange baseline. **10**–**12** are **experimental**; the legacy single-slot helper **`decode_yuv420_srsv2_payload`** returns **`Unsupported`** for **10**–**12** — use **`decode_yuv420_srsv2_payload_managed`**.

## Elementary `.srsv2` file

Starts with the 64-byte sequence header, then repeating framed records: VP packet sync (`PACKET_SYNC` from `libsrs_video`), version/type bytes, `frame_index`, payload length, CRC32 of header fields + payload, payload bytes.

## Decoder requirements

- Ignore reserved trailing bytes in the 64-byte sequence header for schema **1** (decoders read defined offsets only); encoders should zero-fill unused slots.
- Reject unknown sequence schema version.
- Enforce `MAX_FRAME_PAYLOAD_BYTES`, dimension caps, and CRC mismatches as hard errors.
- **FR2** revisions **1**–**4** remain the integer-MV baseline; **5** and **6** add **half-pel** luma MVs (experimental). **3** and **4** add optional entropy-coded intra/P residuals; **7**–**9** add optional **block `qp_delta`** with adaptive residuals (see `docs/srsv2_codec.md`).
