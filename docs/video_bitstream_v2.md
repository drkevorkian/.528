# SRSV2 bitstream and container mapping

**Container policy:** New `.528` **video** tracks should use **`codec_id == 3`** (SRSV2) with the 64-byte sequence header embedded in track config. **`codec_id == 1`** (SRSV1) is legacy; players and tools still open and decode it. **`codec_id == 2`** is **audio** (SRSA), not SRSV2 — see `libsrs_container::codec_ids`. The logical enum **`SrsElementaryVideoCodecId`** (alias `SrsVideoCodecId`) in `libsrs_video` uses the same numeric **values** as video container IDs **1** and **3**, but is **not** the mux `codec_id` field type.

## Sequence header (64 bytes, fixed)

- Magic `SRS2` (4 bytes).
- Schema byte `1`.
- Width / height: `u32` LE each (must satisfy decoder caps in `libsrs_video::srsv2::limits`).
- **Profile** byte (see `SrsVideoProfile` in `libsrs_video::srsv2::model`): **0** Baseline, **1** Main, **2** Pro, **3** Lossless, **4** Screen, **5** Ultra, **6** Research — semantics in **`docs/srsv2_design_targets.md`**.
- Pixel format, color primaries, transfer, matrix, chroma siting, range, loop-filter disable flag, max reference frames.

Embedded verbatim in `.528` **video track config** when `codec_id == 3`.

## Frame payload (mux packet bytes)

### Revision 1 — intra (`FR2\x01`)

Prefix `FR2\x01`, `frame_index` LE `u32`, `qp` byte, then three length-prefixed plane bitstreams (Y, U, V) for YUV420p8 intra.

### Revision 2 — experimental P (`FR2\x02`)

Prefix `FR2\x02`, `frame_index`, `qp`, then per **16×16** macroblock (coverage requires width/height divisible by 16): `mv_x`, `mv_y` (`i16` LE, bounded by `MAX_MOTION_VECTOR_PELS`), `pattern` byte (four bits mark skip for four **8×8** Y sub-blocks), then optional length-prefixed residual blobs for non-skipped sub-blocks using **legacy explicit** intra-style coefficient tuples per **8×8** block. Chroma U/V are predicted by copying the reference planes with half-resolution MVs (no chroma residual in this slice). Decode requires `max_ref_frames ≥ 1` and a valid reference frame (`PFrameWithoutReference` otherwise).

### Revision 3 — intra with adaptive residual coding (`FR2\x03`) — experimental

Prefix matches **`FR2\x03`**. Same top-level layout as revision **1** (`frame_index`, `qp`, three length-prefixed Y/U/V plane blobs). Within each plane, **8×8** blocks are packed as: prediction **mode** byte, **DC** (`i16` LE), then a **tag** byte selecting **explicit AC tuples** (same syntax as rev **1** plane bodies) or **static rANS**-packed AC token stream (`sym_count` `u16` LE, `byte_len` `u16` LE, bytes). Encoders pick the smaller representation per block when `residual_entropy = auto`; `explicit` forces tuples; `rans` forces entropy where legal. **Rev 1** payloads remain the canonical **explicit-tuple-only** intra format; **rev 3** is an optional compression improvement path.

### Revision 4 — P-frame with adaptive residuals (`FR2\x04`) — experimental

Same macroblock grid and motion syntax as revision **2**. Non-skipped **8×8** Y residuals use either legacy tuple blobs or an **adaptive** layout byte **`1`** followed by the same per-block explicit vs rANS tagging as intra **rev 3**. **Rev 2** remains the tuple-only P payload for backward compatibility.

## Elementary `.srsv2` file

Starts with the 64-byte sequence header, then repeating framed records: VP packet sync (`PACKET_SYNC` from `libsrs_video`), version/type bytes, `frame_index`, payload length, CRC32 of header fields + payload, payload bytes.

## Decoder requirements

- Reject unknown sequence schema version.
- Enforce `MAX_FRAME_PAYLOAD_BYTES`, dimension caps, and CRC mismatches as hard errors.
- **FR2** revisions **1** and **2** must remain decodable unchanged; **3** and **4** add optional entropy-coded residuals and are **experimental** (see `docs/srsv2_codec.md`).
