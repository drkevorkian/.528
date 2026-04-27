# Agent 2 Log - Native Container & Streaming

## 2026-04-19

### Changed

- `crates/libsrs_container`: SRSM format types, binary codecs, CRC validation, resync scanner.
- `crates/libsrs_mux`: writer for headers, tracks, packets, cues, and final index.
- `crates/libsrs_demux`: reader for metadata, packet iteration, index rebuild, and seek.
- `tests/container`: golden, truncation, and CRC corruption coverage.
- `tools/container_inspector`: file inspection CLI.
- `docs/specs/container_format.md`, `docs/specs/packet_layout.md`, `docs/specs/timing_and_sync.md`.

### Assumptions

- v1 container keeps index at end and periodic cue blocks in-stream for progressive recovery.
- Video and audio codec config blobs are opaque bytes owned by codec-layer specs.
- Packet `sequence` is monotonic and usable for index packet numbering.

### Interfaces depended on

- Codec track IDs and config payload conventions coordinated with Agent 1 and Agent 3.
- CLI/analyzer integrations depend on `DemuxReader` and `MuxWriter` public APIs.

### Tests added

- `tests/container/tests/roundtrip.rs::golden_mux_demux_roundtrip`
- `tests/container/tests/roundtrip.rs::truncated_file_is_detected`
- `tests/container/tests/roundtrip.rs::bad_crc_is_detected`

### Blockers

- None for container scope; future work is tuning for large-file seek performance.
