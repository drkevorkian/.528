# `.528` container format (v2)

This document matches the implementation in `libsrs_container`, `libsrs_mux`, and `libsrs_demux` as of this revision.

## Endianness

All multi-byte integers are **little-endian**.

## File layout

1. **File header** (see below)
2. **Track table**: `track_count` consecutive **track descriptors**
3. **Blocks**: repeating `SBLK` envelopes until end-of-file (packets, optional cue blocks, final index)

Recommended filename extension: **`.528`**. Legacy files may use **`.srsm`** (same bitstream).

## File header

### v2 (current writer output)

| Offset | Size | Field |
|--------|------|--------|
| 0 | 8 | Magic `SRS528\0\0` (`[b'S',b'R',b'S',b'5',b'2',b'8',0,0]`) |
| 8 | 2 | `version` — must be `2` |
| 10 | 2 | `flags` — low 4 bits: `FileProfile` (lossless, visual, audio-only, video-only, mixed) |
| 12 | 4 | `header_len` — must be **24** (total on-disk header size including magic) |
| 16 | 2 | `track_count` |
| 18 | 2 | reserved (0) |
| 20 | 4 | `cue_interval_packets` (0 disables periodic cue blocks) |

### v1 (legacy read support)

| Offset | Size | Field |
|--------|------|--------|
| 0 | 4 | Magic `SRSM` |
| 4 | 16 | Same field layout as v2 rows 2–7 (`header_len` must be **20**) |

Parsers reject inconsistent pairs (for example v2 magic with `version != 2`, or `SRSM` with `version != 1`).

## Track descriptor

| Field | Type | Notes |
|-------|------|--------|
| `track_id` | u16 | Application-defined |
| `kind` | u8 | `1` audio, `2` video, `3` data, `4` subtitle, `5` metadata, `6` attachment |
| reserved | u8 | 0 |
| `codec_id` | u16 | See `libsrs_contract` / `libsrs_app_services` mapping for native codecs |
| `flags` | u16 | Track-specific |
| `timescale` | u32 | Ticks per second for PTS/DTS interpretation |
| `config_len` | u32 | Length of following blob |
| `config` | bytes | Codec private data (capped: `MAX_TRACK_CONFIG_BYTES`) |

## Block envelope (`SBLK`)

Each logical block is wrapped:

| Field | Size |
|-------|------|
| Magic | 4 (`SBLK`) |
| `block_type` | u8 (`1` packet, `2` cue, `3` index) |
| `flags` | u8 |
| reserved | u16 (0) |
| `body_len` | u32 (capped: `MAX_BLOCK_BODY_BYTES`) |
| `body_crc32` | u32 — **CRC-32C** for v2 files, **IEEE CRC-32** for v1 |
| `header_crc32` | u32 — checksum of the preceding 16 bytes, same algorithm as body |

The **body** immediately follows and must match `body_crc32` under the selected algorithm.

## Packet block body

| Field | Type |
|-------|------|
| `track_id` | u16 |
| `flags` | u16 — includes `KEYFRAME`, `CONFIG`, `DISCONTINUITY`, `DISCARDABLE`, `CORRUPT`, `ENCRYPTED_RESERVED` |
| `sequence` | u64 |
| `pts` | u64 |
| `dts` | u64 |
| `payload_len` | u32 (capped: `MAX_PACKET_PAYLOAD_BYTES`) |
| `payload` | bytes |

Native **SRS audio** packets (`libsrs_audio`) carry a self-contained frame: the bytes in `payload` are exactly the **frame payload** described in [Audio bitstream](specs/audio_bitstream.md) (sample count, channels, then v1 sections or **v2 `R2`** channel blobs). Video packets similarly follow the native video elementary format. Demuxers must not strip or reinterpret inner headers unless a higher layer documents it.

## Cue and index blocks

Both carry a table of **index entries** (28 bytes each):

| Field | Type |
|-------|------|
| `packet_number` | u64 |
| `file_offset` | u64 |
| `track_id` | u16 |
| `flags` | u16 |
| `pts` | u64 |

Cue blocks prefix entries with `cue_id`, `first_packet_number`, and `count`.  
Index blocks prefix with `count` only. Entry count is capped by `MAX_INDEX_ENTRIES_PER_BLOCK` and by `body_len`.

## Security limits (parser hardening)

Central constants in `libsrs_container::format`:

- `MAX_TRACKS`, `MAX_TRACK_CONFIG_BYTES`, `MAX_PACKET_PAYLOAD_BYTES`, `MAX_BLOCK_BODY_BYTES`, `MAX_INDEX_ENTRIES_PER_BLOCK`

Oversized lengths fail with structured `ReadError::LimitExceeded` or `InvalidLength` without unbounded allocation.

## Compatibility

- **Writers** emit v2 (`.528` magic, CRC-32C).
- **Readers** accept v1 (`SRSM`, CRC-32) and v2.
