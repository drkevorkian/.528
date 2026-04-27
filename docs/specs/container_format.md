# SRSM Native Container Format

This document defines the `.srsm` native container used by `libsrs_container`, `libsrs_mux`, and `libsrs_demux`.

## Design Goals

- Original, versioned binary format.
- Deterministic little-endian encoding.
- Stream-safe muxing with periodic cue points.
- Fast seek through terminal index block.
- Recovery hooks for corrupted payload regions.

## Endianness Policy

All integer fields are encoded as little-endian (`LE`) regardless of host architecture.

## Top-Level File Structure

1. File Header (`20 bytes`, fixed).
2. Track Descriptors (`track_count` entries, variable).
3. Interleaved Block Stream:
   - Packet blocks (`BlockType = 1`)
   - Periodic cue blocks (`BlockType = 2`)
   - Final index block (`BlockType = 3`, terminal summary)

## Versioning

- `magic`: `SRSM` (`0x53 0x52 0x53 0x4D`)
- `version`: `1` for the initial stable native container.
- Parsers must reject unsupported versions.

## File Header Layout (20 bytes)

| Offset | Size | Type | Field | Notes |
|---|---:|---|---|---|
| 0 | 4 | `[u8;4]` | `magic` | Must be `SRSM` |
| 4 | 2 | `u16` | `version` | Current = `1` |
| 6 | 2 | `u16` | `flags` | Reserved for future file options |
| 8 | 4 | `u32` | `header_len` | Current fixed value: `20` |
| 12 | 2 | `u16` | `track_count` | Number of track descriptors following |
| 14 | 2 | `u16` | `reserved` | Must be zero |
| 16 | 4 | `u32` | `cue_interval_packets` | Packet cadence for cue emission |

## Track Descriptor Layout (16 + N bytes)

| Offset | Size | Type | Field |
|---|---:|---|---|
| 0 | 2 | `u16` | `track_id` |
| 2 | 1 | `u8` | `kind` (`1=audio`, `2=video`, `3=data`) |
| 3 | 1 | `u8` | `reserved` |
| 4 | 2 | `u16` | `codec_id` |
| 6 | 2 | `u16` | `flags` |
| 8 | 4 | `u32` | `timescale` |
| 12 | 4 | `u32` | `config_len` |
| 16 | N | `[u8]` | `config` (codec/private bytes) |

Track descriptors are contiguous and appear immediately after the file header.
