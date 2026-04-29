# SRS Audio Bitstream (`.srsa`)

Elementary audio format implemented in `libsrs_audio`. Packets are embedded in `.528` / native containers as **opaque payloads**; this spec describes the payload layout and the 16-byte stream header used when storing or muxing a standalone `.srsa` stream.

## Goals

- **Lossless** mono/stereo PCM round-trip (implementation: `i16` interleaved).
- **Versioned** stream and packet headers (`1` = legacy residual coding, `2` = LPC + rANS channel blocks).
- **CRC32 (IEEE)** over each frame packet metadata + payload (see below).

## Stream header (16 bytes)

Writer output uses **`version = 2`**. Readers accept **`1`** or **`2`**.

| Offset | Size | Field | Type | Notes |
|-----:|---:|---|---|---|
| 0 | 4 | `magic` | bytes | ASCII `SRSA` |
| 4 | 1 | `version` | u8 | `1` legacy payload framing; **`2`** current (LPC + rANS channel format) |
| 5 | 1 | `channels` | u8 | `1` or `2` |
| 6 | 2 | `reserved` | bytes | `0` |
| 8 | 4 | `sample_rate` | u32 LE | ticks per second |
| 12 | 4 | `reserved` | bytes | `0` |

Packet **`version`** (frame header, byte offset 2) **must equal** this stream `version`.

## Frame packet header (20 bytes, prefix before payload)

| Offset | Size | Field | Type | Notes |
|-----:|---:|---|---|---|
| 0 | 2 | `sync` | bytes | ASCII `AP` |
| 2 | 1 | `version` | u8 | Same value as stream header `version` (`1` or `2`) |
| 3 | 1 | `channels` | u8 | Must match stream header |
| 4 | 4 | `frame_index` | u32 LE | monotonic |
| 8 | 4 | `sample_count_per_channel` | u32 LE | samples per channel in this frame |
| 12 | 4 | `payload_len` | u32 LE | length of following payload |
| 16 | 4 | `crc32` | u32 LE | CRC32 over `[version, channels, frame_index, sample_count_per_channel, payload_len, payload]` (IEEE polynomial, `crc32fast` in tree) |

## Frame payload (common prefix)

| Offset | Size | Field | Type | Notes |
|-----:|---:|---|---|---|
| 0 | 4 | `sample_count_per_channel` | u32 LE | Redundant check against packet header |
| 4 | 1 | `channels` | u8 | Must match packet |

What follows depends on **version** (and on decoding rules below).

---

## v1 payload (legacy lossless residual)

After the common prefix, **`channel sections`** start at **byte offset 5** (no magic).

Each channel section:

| Field | Type | Notes |
|-----|---|-----|
| `channel_payload_len` | u32 LE | Byte size of the following channel block |
| `first_sample` | i16 LE | Absolute first sample |
| `tokens...` | bytes | Residual token stream for remaining samples |

### Residual token coding (per channel)

- Predictor is previous reconstructed sample.
- Delta in `i32`: `current - previous`.
- Tokens:
  - `0..127`: zero run of length `token + 1`
  - `128`: literal `i32` delta (4 bytes LE) follows
  - `129..255`: small delta `delta = token - 192` (range `-63..63`, excluding `0`)

Decode fails if a sample leaves **`i16`** range.

---

## v2 payload (LPC + rANS, magic `R2`)

After the common prefix:

| Offset | Size | Field | Type | Notes |
|-----:|---:|---|---|---|
| 5 | 2 | `magic` | bytes | **`0x52 0x32`** (ASCII `R2`) — marks v2 channel blob layout |

Channel list begins at **byte offset 7**. For each channel:

| Field | Type | Notes |
|-----|---|-----|
| `channel_len` | u32 LE | Length of the following channel block |
| `channel_blob` | bytes | See below |

### Channel blob

**Mode byte** (`channel_blob[0]`):

| Mode | Meaning |
|---:|---|
| `0` | **Raw PCM**: `u32 LE` byte length (must equal `sample_count * 2 * sizeof(i16)`), then little-endian `i16` samples. |
| `1` | **LPC + rANS residual**: `u8` predictor order `p` in `1..=8`; `p` × **i16 LE** coefficients (Q12 fixed-point taps); `p` × **i16 LE** warmup samples (original prefix); `u32 LE` `rans_len`; **`rans_len` bytes** of rANS-compressed residual bytes. |

Residual order: for sample indices `p..N`, prediction uses fixed-point dot product of quantized coefficients with prior samples, then residual `sample - pred` is encoded. Residuals are packed as **little-endian `i16`**, then each byte is an rANS symbol (alphabet 256).

### rANS (reference: `libsrs_bitio`)

- 32-bit state, `RANS_SCALE = 4096` (12-bit frequency resolution).
- **Model**: uniform symbol frequencies over 256 bytes (encoder and decoder must use the **same** table; streams do not embed a histogram today).
- Stream layout: **4-byte little-endian final state**, then renormalization bytes in **decoder read order** (see implementation).
- Decoders enforce a step budget and **reject trailing bytes** after the last symbol (tight stream).

LPC details (autocorrelation, Levinson–Durbin, max order 8) live in `crates/libsrs_audio/src/lpc.rs` and `codec.rs`.

---

## Decoding API and ambiguity

- **`decode_frame(...)`** (heuristic): if bytes `5..7` equal **`R2`**, parses **v2** body from offset 7; otherwise treats bytes from offset **5** as **v1** sections.
  - **Collision**: a v1 payload could theoretically have a first channel length whose **low 16 bits** are `0x3252`, matching `R2`. That would mis-route heuristic decode.
- **`decode_frame_with_stream_version(..., stream_version)`**: uses the **16-byte stream header** `version`:
  - **`1`**: always legacy framing from byte **5** (ignores magic at 5–6).
  - **`2`**: requires **`R2`** at 5–6 or decode fails.

**`AudioStreamReader`** uses `decode_frame_with_stream_version` with the header `version`, so elementary `.srsa` files are safe. **Muxed-only payloads** produced by `encode_frame` are always v2 with real `R2`; plain **`decode_frame`** is sufficient when no v1 collision is possible.

---

## Extension points

- Non-uniform rANS tables (histogram in-band or shared rule) for better compression.
- More channel layouts when `channels` and policy allow.
- Higher sample widths (`i24`, float) via new stream `version` and header `reserved` fields.
