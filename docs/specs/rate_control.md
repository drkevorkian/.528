# SRS v1 Rate Control Notes

v1 codecs are deterministic and correctness-first. Rate control is intentionally simple and stateless.

## Video v1

- Intra-only coding (`I` frames) with fixed 8x8 blocks.
- No quantizer in v1; residuals are lossless in `u8` sample domain.
- Effective bitrate is controlled externally by:
  - frame size (`width * height`)
  - frame cadence
  - content complexity (token entropy)

### v2 extension path

- Add per-frame quality target and scalar quantization in payload header extension.
- Add predictive frame types (`P`) and GOP controls.
- Add VBV-style buffering model and frame-level target bits.

## Audio v1

- Lossless residual coding with per-channel delta + RLE token stream.
- No psychoacoustic quantization in v1.
- Effective bitrate depends on channel count, sample rate, and signal predictability.

### v2 extension path

- Add optional lossy mode flag in stream header reserved bytes.
- Add block-level LPC order signaling and quantized residual buckets.
- Add CBR/ABR mode signaling with target kbps field.

## Determinism requirements

- Same input samples must produce byte-identical bitstreams in deterministic mode.
- Decoder behavior must be platform-independent for valid streams.
- CRC32 failures are hard errors and must not be silently ignored.
