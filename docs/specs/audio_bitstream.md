# SRS Audio Bitstream v1 (`.srsa`)

This document defines the v1 lossless elementary audio stream for `libsrs_audio`.

## Goals

- Deterministic sample-perfect decode.
- Mono and stereo support.
- CRC32 per packet.

## Stream Header (16 bytes)

| Offset | Size | Field | Type | Notes |
|---|---:|---|---|---|
| 0 | 4 | `magic` | bytes | ASCII `SRSA` |
| 4 | 1 | `version` | u8 | `1` |
| 5 | 1 | `channels` | u8 | `1` or `2` |
| 6 | 2 | `flags_reserved` | bytes | must be `0` in v1 |
| 8 | 4 | `sample_rate` | u32 LE | stream sample rate |
| 12 | 4 | `reserved` | bytes | `0` in v1 |

## Frame Packet Header (20 bytes)

| Offset | Size | Field | Type | Notes |
|---|---:|---|---|---|
| 0 | 2 | `sync` | bytes | ASCII `AP` |
| 2 | 1 | `version` | u8 | `1` |
| 3 | 1 | `channels` | u8 | must match stream header |
| 4 | 4 | `frame_index` | u32 LE | monotonically increasing |
| 8 | 4 | `sample_count_per_channel` | u32 LE | samples per channel in this frame |
| 12 | 4 | `payload_len` | u32 LE | bytes of payload |
| 16 | 4 | `crc32` | u32 LE | CRC over `[version, channels, frame_index, sample_count_per_channel, payload_len, payload]` |

## Frame Payload (lossless residual)

| Offset | Size | Field | Type | Notes |
|---|---:|---|---|---|
| 0 | 4 | `sample_count_per_channel` | u32 LE | redundant integrity field |
| 4 | 1 | `channels` | u8 | integrity field |
| 5 | ... | `channel sections` | mixed | one section per channel |

Each channel section:

| Field | Type | Notes |
|---|---|---|
| `channel_payload_len` | u32 LE | byte size of encoded channel block |
| `first_sample` | i16 LE | absolute starting sample |
| `tokens...` | bytes | residual token stream for remaining samples |

### Residual token coding (per channel)

- Predictor is previous reconstructed sample.
- Delta is computed as `current - previous` (`i32` domain).
- Tokens:
  - `0..127`: zero run of length `token + 1`
  - `128`: literal `i32` delta follows (4 bytes LE)
  - `129..255`: small delta `delta = token - 192` (`-63..63`, excluding `0`)

Decoder errors if reconstructed sample exits `i16` range.

## Extension points

- Reserved bytes in stream header can encode sample format (`i24`, `f32`) in v2+.
- Token marker `129..255` space can be repartitioned to new entropy classes if `version` changes.
- Additional channel layouts (5.1+) can be added by extending allowed `channels` values.
