# SRSV2 bitstream and container mapping

## Sequence header (64 bytes, fixed)

- Magic `SRS2` (4 bytes).
- Schema byte `1`.
- Width / height: `u32` LE each (must satisfy decoder caps in `libsrs_video::srsv2::limits`).
- Profile, pixel format, color primaries, transfer, matrix, chroma siting, range, loop-filter disable flag, max reference frames.

Embedded verbatim in `.528` **video track config** when `codec_id == 3`.

## Frame payload (mux packet bytes)

Prefix `FR2\x01`, `frame_index` LE `u32`, `qp` byte, then three length-prefixed plane bitstreams (Y, U, V) for YUV420p8 intra.

## Elementary `.srsv2` file

Starts with the 64-byte sequence header, then repeating framed records: VP packet sync (`PACKET_SYNC` from `libsrs_video`), version/type bytes, `frame_index`, payload length, CRC32 of header fields + payload, payload bytes.

## Decoder requirements

- Reject unknown sequence schema version.
- Enforce `MAX_FRAME_PAYLOAD_BYTES`, dimension caps, and CRC mismatches as hard errors.
