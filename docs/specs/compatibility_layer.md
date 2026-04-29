# Compatibility Layer Spec

`libsrs_compat` provides a source abstraction that can be backed by:

- **Stub backend (default):** native file paths and a small synthetic A/V source for tests.
- **FFmpeg backend (optional, feature `ffmpeg`):** arbitrary containers and codecs exposed through the same traits.

## Interfaces

- **`MediaProbe`**: inspect media metadata and tracks (`ProbeResult`).
- **`MediaIngestor`**: open a path, read `SourcePacket` values (contract `Packet` + optional byte offset), seek by wall-clock ms, close.
- **`CompatLayer`**: selects the backend and constructs boxed `MediaProbe` / `MediaIngestor` instances.

### `ProbeResult` and `CompatTrackInfo`

Each track exposes:

- `id`, `kind`, `codec`, `role`, optional `language`.
- **`audio_sample_rate` / `audio_channels`** (`Option`): filled when the backend knows them—for example:
  - **Native `.528` / `.srsm`:** parsed from the audio track **descriptor `config`** (sample rate as `u32` LE, then channel count `u8`; mono/stereo only in paths that mux native SRS audio).
  - **Standalone `.srsa`:** read from the **16-byte** `AudioStreamHeader` at the start of the file (see [Audio bitstream](audio_bitstream.md)).
  - **Synthetic stub source** (`stub-synthetic-av`): fixed **48 kHz, mono** to match stub packet timing.
  - **FFmpeg:** from `AVCodecParameters` for audio streams (optional build); channel counts above **2** are not propagated for native import (see app-layer policy below).

## Stub backend behavior

By extension:

- **`.528` / `.srsm`:** demux, probe tracks, ingest packets with native timestamps.
- **`.srsv` / `.srsa`:** elementary native video/audio streams.
- **Anything else:** synthetic **stub-synthetic-av** probe (two tracks: native video + native audio) and alternating video/audio packets for pipeline tests.

## Import path (CLI / `libsrs_app_services`)

Native **import** reads all packets from the ingestor, builds mux **track descriptors** from the probe (including audio rate/channels when present), then encodes through **`libsrs_mux`**, **`libsrs_video`**, and **`libsrs_audio`**—not a placeholder tiled frame path. If probe does not supply audio layout, the importer assumes **48 kHz mono**; **mono/stereo** is required for native SRS audio mux.

## Non-goals

- No direct codec/container business logic beyond glue and parsing helpers.
- No forced dependency on FFmpeg for base builds.

## Codec Policy

Codec detection is separate from codec permission. See
`docs/specs/supported_codecs.md` for the allowlist of royalty-free codecs that
can be played or converted.

## Unsafe code

The optional FFmpeg probe reads `sample_rate` / `channels` from FFmpeg’s `AVCodecParameters` with a **narrow `unsafe` block** localized to `ffmpeg_backend.rs`. Default (non-FFmpeg) builds do not execute that path.
