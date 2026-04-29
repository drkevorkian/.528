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
- `sample.528`: native multiplexed **v2** container (primary extension); legacy files may use `sample.srsm` with the same bitstream.

`libsrs_compat` probes **`.srsa`** and **`.528`** tracks so native **import** can recover **audio sample rate and channels** from stream/container headers when muxing through `libsrs_app_services`.

Re-generate with:

```bash
cargo run -p srs_cli -- encode "samples/sample_video.raw" "samples/sample.srsv"
cargo run -p srs_cli -- encode "samples/sample_audio.pcm16le" "samples/sample.srsa"
cargo run -p srs_cli -- mux "samples/sample" "samples/sample.528"
cargo run -p srs_cli -- analyze "samples/sample.528"
```
