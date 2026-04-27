# SRSM Timing and Synchronization Rules

This document defines timestamp behavior, packet interleave expectations, cue cadence, and seek behavior.

## Timestamp Units

- Each track has a `timescale` from its descriptor.
- `pts` and `dts` in packet headers are encoded as track-local ticks.
- A demuxer must interpret packet timestamps in the context of packet `track_id`.

## Ordering Rules

- Muxers may interleave tracks arbitrarily, but should preserve monotonic `sequence`.
- Per-track decode order should follow `dts`.
- Presentation uses `pts`.

## Sequence Rules

- `sequence` is a container-global monotonic counter assigned by the muxer.
- `packet_number` in index/cue entries maps to the packet sequence slot written by the muxer.

## Cue Scheduling

- `cue_interval_packets` from file header controls periodic cue insertion.
- If `cue_interval_packets = 0`, muxers skip periodic cue blocks and only emit terminal index.
- Cue blocks summarize packets written since the previous cue and provide recovery anchors.

## Seek Semantics

- Primary seek source: final index block.
- Fallback seek source: merged cue entries when final index is missing/corrupt.
- `seek_nearest(target_pts)` selects nearest entry with `entry.pts <= target_pts`.
- Demux seeks reader position to `entry.file_offset`, then resumes block parse.

## Corruption Recovery Hooks

- On parse failure (invalid magic/header CRC/body CRC), demuxer may probe ahead for `SBLK`.
- Resync scan should start after the failed byte to avoid infinite loops.
- Recovered blocks are accepted only if full header/body validation succeeds.

## Sync Best Practices

- Emit keyframe packets regularly and mark with `KEYFRAME`.
- Align cue cadence with keyframe cadence for efficient random access.
- Keep audio and video timelines close in wall-clock terms to reduce A/V skew during seek.
