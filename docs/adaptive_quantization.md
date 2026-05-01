# SRSV2 adaptive quantization (experimental)

This workspace implements **frame-level** adaptive quantization for SRSV2 encode paths: the encoder analyzes **16×16** luma macroblock activity (variance, edge strength, gradients; optional screen-oriented scoring) and derives **one effective QP per frame**, clamped to `[min_qp, max_qp]`.

**Per-macroblock QP deltas are not written to the bitstream** in the current slice. The on-wire frame header still carries a **single QP byte**, as in `docs/video_bitstream_v2.md`. Statistics (`SrsV2AqEncodeStats`) summarize how MB-level suggestions would distribute if block syntax existed later.

Modes (`SrsV2AdaptiveQuantizationMode`): **Off**, **Activity**, **EdgeAware**, **ScreenAware**. Strength and delta bounds live on `SrsV2EncodeSettings` (`aq_strength`, `min_block_qp_delta`, `max_block_qp_delta`). **Do not treat this as production-grade adaptive quantization** — benchmark before claiming quality or bitrate wins.

See also: `docs/rate_control.md`, `docs/srsv2_benchmarks.md`, `bench_srsv2` flags `--aq` and `--aq-strength`.
