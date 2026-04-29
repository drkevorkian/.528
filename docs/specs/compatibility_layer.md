# Compatibility Layer Spec

`libsrs_compat` provides a source abstraction that can be backed by:

- **Stub backend (default):** **native only** — `.528` / `.srsm` multiplexed, `.srsv` video elementary, `.srsa` audio elementary. No synthetic foreign A/V; unknown extensions **fail** with a message pointing to FFmpeg.
- **FFmpeg backend (optional, feature `ffmpeg`):** arbitrary containers and codecs exposed through the same traits.

## Interfaces

- **`MediaProbe`**: inspect media metadata and tracks (`ProbeResult`).
- **`MediaIngestor`**: open a path, read `SourcePacket` values (contract `Packet` + optional byte offset), seek by wall-clock ms, close.
- **`CompatLayer`**: selects the backend and constructs boxed `MediaProbe` / `MediaIngestor` instances.

### `ProbeResult` and `CompatTrackInfo`

Each track exposes:

- `id`, `kind`, `codec`, `role`, optional `language`.
- **`video_width` / `video_height`** (`Option`): for **video** tracks when known (mux track `config`, `.srsv` stream header, or FFmpeg).
- **`audio_sample_rate` / `audio_channels`** (`Option`): for **audio** tracks when known—for example:
  - **Native `.528` / `.srsm`:** parsed from the audio track **descriptor `config`** (sample rate as `u32` LE, then channel count `u8`; mono/stereo for native SRS mux).
  - **Standalone `.srsa`:** read from the **16-byte** `AudioStreamHeader` at the start of the file (see [Audio bitstream](audio_bitstream.md)).
  - **FFmpeg:** from `AVCodecParameters` for audio streams (optional build); channel counts above **2** are not propagated for native import (see app-layer policy below).

## Stub backend behavior

By extension:

- **`.528` / `.srsm`:** demux, probe tracks (**including** video `config` width/height), ingest packets with native timestamps.
- **`.srsv` / `.srsa`:** elementary native video/audio streams (dimensions / audio layout from stream headers).
- **Anything else:** **`MediaProbe::probe_path` / `MediaIngestor::open_path` return `Err`** with `FOREIGN_MEDIA_REQUIRES_FFMPEG` (build with `libsrs_compat` feature `ffmpeg`).

## Import path (CLI / `libsrs_app_services`)

Native **import** reads all packets, **decodes** native payloads to normalized frames (`MediaDecoder` in `libsrs_app_services::import_pipeline`), then **re-encodes** through **`libsrs_mux`**, **`libsrs_video`**, and **`libsrs_audio`** (`NativeEncoderSink`). Muxed packets use codec payloads; `.srsv` / `.srsa` elementary ingests expose **raw** raster / PCM in packets and are normalized the same way. If probe does not supply audio layout, the importer assumes **48 kHz mono** where applicable; **mono/stereo** is required for native SRS audio mux.

## Non-goals

- No direct codec/container business logic beyond glue and parsing helpers.
- No forced dependency on FFmpeg for base builds.

## Codec Policy

Codec detection is separate from codec permission. See
`docs/specs/supported_codecs.md` for the allowlist of royalty-free codecs that
can be played or converted.

## Unsafe code

The optional FFmpeg probe reads `sample_rate` / `channels` / video `width` / `height` from FFmpeg’s `AVCodecParameters` with **narrow `unsafe`** localized to `ffmpeg_backend.rs`. Default (non-FFmpeg) builds do not execute that path.
