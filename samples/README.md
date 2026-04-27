# Samples

This directory stores synthetic and real media samples used by:

- CLI smoke tests
- `tests/e2e` integration checks
- benchmark and fuzz seed generation

Current native bootstrap samples:

- `sample_video.raw`: 64x64 grayscale raw frame source.
- `sample_audio.pcm16le`: mono PCM16LE source samples.
- `sample.srsv`: native video elementary stream.
- `sample.srsa`: native audio elementary stream.
- `sample.srsm`: native multiplexed container generated from the elementary streams.

Re-generate with:

```bash
cargo run -p srs_cli -- encode "samples/sample_video.raw" "samples/sample.srsv"
cargo run -p srs_cli -- encode "samples/sample_audio.pcm16le" "samples/sample.srsa"
cargo run -p srs_cli -- mux "samples/sample" "samples/sample.srsm"
cargo run -p srs_cli -- analyze "samples/sample.srsm"
```
