# SRSV2 codec overview

**SRSV2** (`codec_id` **3** in `.528`, elementary `.srsv2`) is the **default** video codec for **new** `.528` files created by encode, import, transcode, and mux workflows in this workspace (unless callers explicitly select legacy SRSV1). Long-term intent is an **8K-first**, scalable codec competitive with **H.264-class** efficiency at HD–8K — see **`docs/srsv2_design_targets.md`** for profiles, presets policy, and roadmap. **`docs/srsv2_benchmarks.md`** defines how **measurable** comparisons vs **FFmpeg/x264** must be run before claiming bitrate–quality wins.

**SRSV1** (`codec_id` **1** in `.528`, elementary `.srsv`) is **legacy / prototype** compatibility: grayscale intra, still fully readable and writable for tests and older assets.

**SRSV2** is a native bitstream (not MPEG): block prediction, transforms, quantization, and framed residuals. It does **not** interoperate with H.264/HEVC/AV1/VVC elementary streams and does not embed third-party codec sources.

**Default CLI `encode` to `.528`** (square raw → `.srsv2` elementary) is still a **single intra** frame (`FR2\x01`, `max_ref_frames` 0 in the default sequence helper). **Native import** (SRSV2 policy) writes **`max_ref_frames = 1`** and emits **P** frames (`FR2\x02`) on subsequent pictures when width/height are multiples of **16** (otherwise falls back to intra per macroblock grid rules). **P-frame status:** **16×16 integer-pel** motion, bounded MV search (`SrsV2EncodeSettings::motion_search_radius`), skip/residual **Y** blocks, chroma from reference with half MVs; import refreshes the encoder reference with **`decode_yuv420_srsv2_payload`** so the chain matches playback. Sub-pel/B-frames, richer rate control, GPU codecs, and OS audio/video output remain **future slices**.

## Implemented in this repository

- 64-byte `SRS2` sequence header (little-endian fields + profile/pixel/color metadata), including **`max_ref_frames`** (capped; enables reference pictures for **P** prototype).
- YUV420p8 intra frame payloads (`FR2\x01` magic + plane chunks).
- Experimental P-frame payloads (`FR2\x02`): integer MV per 16×16 MB + 8×8 residual codec (`libsrs_video::srsv2::p_frame_codec`).
- Elementary `.srsv2` streams (sync + CRC-framed payloads).
- Container mux/demux with `codec_id == 3` and bounded playback decode for primary video (`decode_yuv420_srsv2_payload`).
- CLI: `encode --codec srsv2`, `analyze --dump-codec`, decode of `.srsv2` to raw YUV via app services.

## Planned / not yet merged

- Half-pel motion, **B**-frames, and richer merged MV modes beyond the integer-pel **P** prototype.
- Full rANS symbol models across all syntax elements (baseline uses structured plane bytes with bounds checks).
- Loop filters beyond optional stubs, HDR signaling beyond sequence fields, 10-bit encode/decode paths.
- GPU backends (`gpu-wgpu`, `gpu-cuda` feature placeholders).

## Security model

Decoders treat all inputs as hostile: capped dimensions, capped payloads, no panics on malformed syntax, structured `SrsV2Error` / container errors. See `docs/video_bitstream_v2.md` for limits.
