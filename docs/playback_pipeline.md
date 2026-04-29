# Playback pipeline (`.528` / `.srsm`)

## Architecture

- **Core type:** `libsrs_app_services::playback::PlaybackSession` opens a file, validates track descriptors up front (dimensions, codec IDs, channel layout), rebuilds the demux index when possible, and exposes:
  - `decode_next_step()` — file-order interleaved A/V (recommended for players).
  - `decode_next_video_frame()` / `decode_next_audio_chunk()` — per-stream pulls with a **bounded** cross-track stash (`MAX_STASH_PACKETS`).
- **Demux:** `DemuxReader` (`libsrs_demux`) over `BufReader<File>`; packet payloads are already capped by container I/O (`MAX_PACKET_PAYLOAD_BYTES`).
- **Decoders:** `libsrs_video::decode_frame` (intra), `libsrs_audio::decode_frame_with_stream_version` with v2 stream payloads.
- **Errors:** `PlaybackError` (thiserror); no panics on malformed bitstreams in the playback path.

## Shared between CLI and UI

- `srs_cli play <file.528>` constructs a `PlaybackSession` after `inspect_media` and runs a bounded decode loop (default `--frames 30`). Flags: `--no-audio`, `--no-video`, `--seek-ms`, `--decode-only`.
- `srs_player` opens a session when you press **Play** (after license/codec policy checks). **Playing** is only set once a session exists and decoding is underway; failed open/decode surfaces a notification and leaves **Playing** false.

## What output is real today

- **Real:** Demux timestamps (PTS/DTS), frame dimensions, payload CRC32C metadata, grayscale **in-panel** egui texture from the last decoded video frame.
- **Not implemented:** OS audio output, GPU presentation, full-screen video sink, multi-format codec matrix.

## Limitations

- Seek requires a **non-empty** demux index after `rebuild_index()`. Files with no indexed packets report `PlaybackError::SeekUnsupported`.
- Only **primary** SRS video (codec id 1) and audio (codec id 2) tracks are decoded; other kinds are skipped deterministically.
- CONFIG packets are skipped for decode; corrupt-flagged packets are rejected.

## Security / hostile inputs

- Overside dimensions and excessive pixel counts are rejected at **open** time.
- Sequence numbers must fit `u32` for decode APIs.
- Unbounded RAM is avoided: stash cap, container payload cap, no “decode whole file into one buffer” in this path.

## Tests

```bash
cargo test -p libsrs_app_services playback_tests
cargo test -p srs_player
```

## Next slices (suggested)

- **Presentation:** CPAL (or platform) audio output + vsync’d frame timing (still CPU decode).
- **Codec breadth:** P‑frames / bitstream-driven `FrameType`, codec ID matrix with explicit unsupported errors.
- **GPU:** decode or color conversion on device behind a narrow trait boundary.
