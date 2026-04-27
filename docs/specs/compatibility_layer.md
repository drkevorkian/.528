# Compatibility Layer Spec

`libsrs_compat` provides a source abstraction that can be backed by:

- stub/native-safe placeholder backend (default)
- FFmpeg backend (optional, feature `ffmpeg`)

## Interfaces

- `MediaProbe`: inspect media metadata and streams.
- `MediaIngestor`: open source, read packets, seek, close.
- `CompatLayer`: backend selector and object factory.

## Non-goals

- No direct codec/container business logic.
- No forced dependency on FFmpeg for base builds.

## Codec Policy

Codec detection is separate from codec permission. See
`docs/specs/supported_codecs.md` for the allowlist of royalty-free codecs that
can be played or converted.
