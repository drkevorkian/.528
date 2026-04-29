# SRSV2 codec overview

**SRSV1** (`codec_id` **1** in `.528`, elementary `.srsv`) remains the legacy grayscale intra prototype used for conformance and simple tests.

**SRSV2** (`codec_id` **3** in `.528`, elementary `.srsv2`) is the modern CPU-first path: block intra prediction, separable transforms, scalar quantization, and framed entropy-coded residuals. It does **not** interoperate with H.264/HEVC/AV1/VVC bitstreams and does not embed third-party codec sources.

## Implemented in this repository

- 64-byte `SRS2` sequence header (little-endian fields + profile/pixel/color metadata).
- YUV420p8 intra frame payloads (`FR2\x01` magic + plane chunks).
- Elementary `.srsv2` streams (sync + CRC-framed payloads).
- Container mux/demux with `codec_id == 3` and bounded playback decode for primary video.
- CLI: `encode --codec srsv2`, `analyze --dump-codec`, decode of `.srsv2` to raw YUV via app services.

## Planned / not yet merged

- Inter prediction (P/B), half-pel, merged MV modes.
- Full rANS symbol models across all syntax elements (baseline uses structured plane bytes with bounds checks).
- Loop filters beyond optional stubs, HDR signaling beyond sequence fields, 10-bit encode/decode paths.
- GPU backends (`gpu-wgpu`, `gpu-cuda` feature placeholders).

## Security model

Decoders treat all inputs as hostile: capped dimensions, capped payloads, no panics on malformed syntax, structured `SrsV2Error` / container errors. See `docs/video_bitstream_v2.md` for limits.
