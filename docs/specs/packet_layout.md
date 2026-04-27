# SRSM Packet and Block Layout

This document defines block framing, packet payload blocks, cue blocks, and index blocks.

## Block Envelope (20 bytes)

Every stream block starts with a fixed-size envelope:

| Offset | Size | Type | Field | Description |
|---|---:|---|---|---|
| 0 | 4 | `[u8;4]` | `block_magic` | `SBLK` |
| 4 | 1 | `u8` | `block_type` | `1=packet`, `2=cue`, `3=index` |
| 5 | 1 | `u8` | `flags` | Block-level flags (reserved) |
| 6 | 2 | `u16` | `reserved` | Must be zero |
| 8 | 4 | `u32` | `body_len` | Size of following body bytes |
| 12 | 4 | `u32` | `body_crc32` | CRC32 over body bytes |
| 16 | 4 | `u32` | `header_crc32` | CRC32 over envelope bytes `[0..16)` |

Parsing requirements:

- Validate `block_magic`.
- Validate `header_crc32` before reading body.
- Read `body_len`, then validate `body_crc32`.

## Packet Block Body (32 + payload bytes)

| Offset | Size | Type | Field |
|---|---:|---|---|
| 0 | 2 | `u16` | `track_id` |
| 2 | 2 | `u16` | `flags` |
| 4 | 8 | `u64` | `sequence` |
| 12 | 8 | `u64` | `pts` |
| 20 | 8 | `u64` | `dts` |
| 28 | 4 | `u32` | `payload_len` |
| 32 | N | `[u8]` | `payload` |

Defined packet flags:

- `0x0001`: keyframe
- `0x0002`: config packet
- `0x0004`: discontinuity

## Index Entry Layout (28 bytes)

| Offset | Size | Type | Field |
|---|---:|---|---|
| 0 | 8 | `u64` | `packet_number` |
| 8 | 8 | `u64` | `file_offset` |
| 16 | 2 | `u16` | `track_id` |
| 18 | 2 | `u16` | `flags` |
| 20 | 8 | `u64` | `pts` |

## Cue Block Body (20 + M*28 bytes)

| Offset | Size | Type | Field |
|---|---:|---|---|
| 0 | 8 | `u64` | `cue_id` |
| 8 | 8 | `u64` | `first_packet_number` |
| 16 | 4 | `u32` | `entry_count` |
| 20 | M*28 | bytes | `index_entries` |

Cue blocks are periodic recovery/seek breadcrumbs embedded in the media stream.

## Final Index Block Body (4 + M*28 bytes)

| Offset | Size | Type | Field |
|---|---:|---|---|
| 0 | 4 | `u32` | `entry_count` |
| 4 | M*28 | bytes | `index_entries` |

The final index is authoritative for complete-file seek resolution.

## Corruption Detection and Resync

- CRC mismatch on header/body marks a corrupted block.
- Demuxers may scan forward for the next `SBLK` marker using a byte-window resync helper.
- After resync, parsers must continue strict CRC validation before accepting recovered blocks.
