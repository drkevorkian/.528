# SRS Video Bitstream v1 (`.srsv`)

This document defines the v1 elementary video stream for `libsrs_video`.

## Goals

- Intra-only deterministic decode (`I` frames only in v1).
- Strongly typed frame semantics (`FrameType` extensible for `P` in v2+).
- Packet-level CRC32 validation.

## Stream Header (16 bytes)

| Offset | Size | Field | Type | Notes |
|---|---:|---|---|---|
| 0 | 4 | `magic` | bytes | ASCII `SRSV` |
| 4 | 1 | `version` | u8 | `1` |
| 5 | 3 | `flags_reserved` | bytes | must be `0` in v1 |
| 8 | 4 | `width` | u32 LE | frame width |
| 12 | 4 | `height` | u32 LE | frame height |

## Frame Packet Header (16 bytes)

| Offset | Size | Field | Type | Notes |
|---|---:|---|---|---|
| 0 | 2 | `sync` | bytes | ASCII `VP` |
| 2 | 1 | `version` | u8 | `1` |
| 3 | 1 | `frame_type` | u8 | `0 = I`, `1 = P(reserved)` |
| 4 | 4 | `frame_index` | u32 LE | monotonically increasing per stream |
| 8 | 4 | `payload_len` | u32 LE | bytes of encoded payload |
| 12 | 4 | `crc32` | u32 LE | CRC over `[version, frame_type, frame_index, payload_len, payload]` |

## Frame Payload (v1 intra residual)

| Offset | Size | Field | Type | Notes |
|---|---:|---|---|---|
| 0 | 1 | `block_size` | u8 | fixed `8` in v1 |
| 1 | 4 | `sample_count` | u32 LE | `width * height` |
| 5 | N | `tokens` | bytes | residual token stream, block-raster order |

### Residual prediction and token coding

- Image is processed in 8x8 blocks, raster order.
- Within each block, predictor starts at `128`.
- For each sample in block-raster order:
  - `delta = sample - predictor`
  - predictor becomes reconstructed sample.
- Tokens:
  - `0..127`: zero run of length `token + 1`
  - `128`: literal `i16` delta follows (2 bytes LE)
  - `129..255`: small delta `delta = token - 192` (`-63..63`, excluding `0`)

Decode rejects out-of-range reconstructed samples (`<0` or `>255`).

## Extension points

- `frame_type = 1` reserved for predictive/inter frames (`P`) in future versions.
- Header reserved bytes can carry color format / chroma mode in v2+.
- Payload can switch entropy mode by reserving `block_size = 0` as an extension marker.
