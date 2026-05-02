# Playback pipeline (`.528` / `.srsm`)

## Architecture

- **Core type:** `libsrs_app_services::playback::PlaybackSession` opens a file, validates track descriptors up front (dimensions, codec IDs, channel layout), rebuilds the demux index when possible, and exposes:
  - `decode_next_step()` — file-order interleaved A/V (recommended for players).
  - `decode_next_video_frame()` / `decode_next_audio_chunk()` — per-stream pulls with a **bounded** cross-track stash (`MAX_STASH_PACKETS`).
- **Demux:** `DemuxReader` (`libsrs_demux`) over `BufReader<File>`; packet payloads are already capped by container I/O (`MAX_PACKET_PAYLOAD_BYTES`).
- **Decoders:** primary video by `codec_id`: **`codec_id` 1** → SRSV1 legacy grayscale intra (`libsrs_video::decode_frame`); **`codec_id` 3** → SRSV2 via **`decode_yuv420_srsv2_payload_managed`** and a bounded **`SrsV2ReferenceManager`**: intra **`FR2\x01`** / **`FR2\x03`** / **`FR2\x07`**, experimental **P** **`FR2\x02`**…**`\x09`** when references are available, experimental **B** **`FR2\x0A`**/**`\x0B`** when **`max_ref_frames ≥ 2`** and both temporal anchors are already in the manager (streams often store **decode order** *I₀ → P₂ → B₁*, not presentation order; there is **no** reorder buffer in **`PlaybackSession`** yet). **`FR2\x0A`/`\x0B`** with **`max_ref_frames < 2`** → **`PlaybackError::Unsupported`**. Experimental **alt-ref** **`FR2\x0C`** updates references with **`is_displayable == false`** (no texture advance). **`codec_id` 2** → SRSA (`libsrs_audio::decode_frame_with_stream_version`, v2 stream payloads).
- **Errors:** `PlaybackError` (thiserror); no panics on malformed bitstreams in the playback path.

## Shared between CLI and UI

- `srs_cli play <file.528>` constructs a `PlaybackSession` after `inspect_media` and runs a bounded decode loop (default `--frames 30`). Flags: `--no-audio`, `--no-video`, `--seek-ms`, `--decode-only`.
- `srs_player` opens a session when you press **Play** (after license/codec policy checks). **Playing** is only set once a session exists and decoding is underway; failed open/decode surfaces a notification and leaves **Playing** false.

## What output is real today

- **Real:** Demux timestamps (PTS/DTS), frame dimensions, payload CRC32C metadata, **in-panel** egui texture from the last decoded video frame (grayscale presentation path; SRSV2 frames are decoded as YUV420p8 then displayed).
- **Not implemented:** OS audio output, GPU presentation, full-screen video sink, multi-format codec matrix.

## Primary media codecs (mux)

| `codec_id` | Role | Decoder |
|------------|------|---------|
| **1** | Video — **SRSV1** legacy | Grayscale intra (`libsrs_video::decode_frame`) |
| **2** | Audio — **SRSA** | LPC v2 stream decode (`libsrs_audio`, `STREAM_VERSION_V2`) |
| **3** | Video — **SRSV2** default | Intra YUV420p8 (`FR2\x01` / `FR2\x03` / `FR2\x07`); experimental **P** (`FR2\x02`…`\x09`); experimental **B** (`FR2\x0A`/`\x0B`) when **`max_ref_frames ≥ 2`** + decode-order refs; experimental **alt-ref** (`FR2\x0C`, non-display); **B** + **`max_ref_frames < 2`** → unsupported |

- **SRSV2** (`codec_id` **3**) is the **default** for newly generated `.528` media: 64-byte sequence header in track config; **`PlaybackSession`** keeps a **`SrsV2ReferenceManager`** sized by **`max_ref_frames`** so **P** / **B** / **alt-ref** paths stay bounded and hostile-input safe. **Inter** streams need **seek** to land on a prior **keyframe** and decode forward to rebuild references. **`stop`** / seek prep clears the manager. **Alt-ref** packets do not bump the decoded-display frame counter or refresh the on-screen texture.
- **SRSV1** (`codec_id` **1**) is **legacy**; playback uses the older grayscale intra path.

## Limitations

- Seek requires a **non-empty** demux index after `rebuild_index()`. Files with no indexed packets report `PlaybackError::SeekUnsupported`.
- Only **primary** SRS video (**SRSV2** `codec_id` 3 or **SRSV1** `codec_id` 1) and audio (`codec_id` 2) tracks are decoded; other kinds are skipped deterministically.
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
