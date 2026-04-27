# Supported Codec Policy

SRS supports playback and conversion for codecs that do not require external
playback patent/licensing arrangements for this project.

## Allowed Codecs

Video:

- SRS Native Video
- AV1
- VP9
- VP8
- Theora

Audio:

- SRS Native Audio
- Opus
- Vorbis
- FLAC
- Speex
- PCM

## Blocked By Policy

These codecs may be detectable by compatibility backends, but they are not
enabled for playback/conversion under the no-license policy:

- H.264 / AVC
- H.265 / HEVC
- AAC

## Unknown Codecs

Unknown codecs are treated as unsupported until explicitly reviewed and added
to the allowlist.

## Compatibility Backend

When the optional FFmpeg backend is enabled, SRS may inspect many media files.
Detection does not imply that a codec is allowed for playback/conversion. The
final allow/deny decision comes from `CodecType::is_royalty_free_playback_allowed`.
