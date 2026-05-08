# Next HEVC-Class Codec Implementation Block

**Source gate:** [`windows_hevc_progress_results.md`](windows_hevc_progress_results.md)

**Selected feature:** **C. context-adaptive residual coefficient entropy**

**Gate evidence:** `inter_residual_bytes` is the largest named byte bucket: **4058 / 4949 bytes** on `scene_cut/SRSV2-pc-fixed16x16` (**82%** of that row). StaticV1 vs ContextV1 MV entropy does not solve this; the next block should target residual coefficient entropy directly.

---

## Cursor Block

```text
BLOCK 6 GOAL:
Implement context-adaptive residual coefficient entropy as the next HEVC-class architecture layer.

SOURCE:
docs/windows_hevc_progress_results.md selected:
C. context-adaptive residual coefficient entropy

WHY:
- Biggest named byte bottleneck is inter_residual_bytes.
- Gate row: scene_cut/SRSV2-pc-fixed16x16 total_bytes=4949, inter_residual_bytes=4058.
- StaticV1/ContextV1 MV entropy is not enough; coefficient residual bytes dominate.
- Do not claim SRSV2 beats H.265/HEVC/x265.

WORK IN COMPLETE MODULES:
- crates/libsrs_video/src/srsv2/residual_context_entropy.rs
- crates/libsrs_video/src/srsv2/residual_entropy.rs
- crates/libsrs_video/src/srsv2/frame_codec.rs
- crates/libsrs_video/src/srsv2/p_frame_codec.rs
- crates/libsrs_video/src/srsv2/rate_control.rs
- crates/libsrs_video/src/srsv2/mod.rs
- tools/quality_metrics/src/bin/bench_srsv2.rs
- docs/residual_context_entropy.md

NON-NEGOTIABLES:
1. Security first: all decoders must reject malformed streams with structured errors; no panics on hostile input.
2. Backward compatibility: existing residual modes and all old FR2 revisions must still decode.
3. Default behavior must not change unless explicitly requested by a new flag or setting.
4. No tiny scattered edits: create the new module and route through it cleanly.
5. No H.265 superiority claim anywhere.
6. No x265 comparison claims; this is internal residual entropy work.

CREATE COMPLETE MODULE:
crates/libsrs_video/src/srsv2/residual_context_entropy.rs

MODULE MUST CONTAIN:
- rustdoc explaining:
  - this is coefficient residual entropy, not MV entropy
  - it is experimental
  - it is not CABAC
  - it is not a claim of HEVC parity
  - malformed input policy
- Public types:
  - ResidualContextModel
  - ResidualContextId
  - ResidualContextSymbol
  - ResidualContextStats
  - ResidualContextError
  - ResidualContextEncodeResult
  - ResidualContextDecodeResult
- Public API:
  - encode_residual_context_v1(...)
  - decode_residual_context_v1(...)
  - estimate_residual_context_v1_bytes(...)
  - validate_residual_context_stream(...)
- Coefficient contexts:
  - plane context: Y / U / V
  - block position context: DC-ish / low-frequency / high-frequency buckets
  - zero-run context
  - sign context
  - magnitude bucket context
- Bounds:
  - max symbols per block
  - max encoded bytes per block/frame
  - max zero-run length
  - checked length prefixes
  - checked symbol counts
- Determinism:
  - same input coefficients must produce identical bytes
  - context selection must be purely derived from block metadata and coefficient position

WIRE / FORMAT PLAN:
- Add a documented experimental residual context stream format in docs/residual_context_entropy.md.
- Use a small magic/version for the residual context blob, for example:
  - magic: S2RC
  - version: 1
- The module owns only the residual-context payload blob.
- Integration into FR2 must be explicitly gated by a new residual mode.
- If a new FR2 revision is required, document the revision plan before adding constants.
- Existing FR2 residual modes must remain byte-for-byte compatible.

RATE CONTROL / SETTINGS:
- Extend residual mode parsing carefully:
  - existing: explicit, auto, rans
  - add: context or context-v1
- Default remains auto.
- Auto must not choose context-v1 unless it is smaller than the existing explicit/rANS choice for that block or frame.
- Forced context-v1 is allowed for testing/measurement but must record failures cleanly.

ENCODER INTEGRATION:
- In intra and P residual paths:
  - keep existing explicit and rANS encoders intact
  - add context-v1 as an additional candidate
  - when residual mode is auto, choose the smallest valid representation
  - when residual mode is context/context-v1, force the new path
- Track telemetry:
  - residual_context_blocks
  - residual_context_bytes
  - residual_context_symbols
  - residual_context_zero_runs
  - residual_context_fallback_blocks
  - residual_context_failure_reason when forced mode fails
- Do not alter motion search, partition decisions, CTU reporting, x264/x265 helpers, or benchmark math.

DECODER INTEGRATION:
- Add decode branch only for the new gated residual payload/revision.
- Decoder must reject:
  - bad magic
  - unknown version
  - truncated header
  - truncated symbol stream
  - symbol count overflow
  - zero-run beyond block coefficient capacity
  - invalid context id
  - magnitude overflow
  - trailing garbage
- Existing old streams must still decode.

BENCHMARK:
Update tools/quality_metrics/src/bin/bench_srsv2.rs.

ADD FLAGS:
- --compare-residual-context

BEHAVIOR:
- Runs comparable passes on the same input:
  - SRSV2-residual-auto-current
  - SRSV2-residual-context-v1
  - SRSV2-residual-auto-with-context
- No FFmpeg required.
- No x265 required.
- If context-v1 forced mode fails, emit an error row without aborting sibling rows.

REPORT FIELDS:
- residual_context_status
- residual_context_blocks
- residual_context_bytes
- residual_context_symbols
- residual_context_zero_runs
- residual_context_fallback_blocks
- residual_context_failure_reason
- inter_residual_bytes
- intra residual context bytes if available
- total_bytes
- PSNR-Y
- SSIM-Y
- encode/decode FPS

DOCS:
Create docs/residual_context_entropy.md.

DOC MUST INCLUDE:
- Purpose and non-goals.
- Wire/blob format.
- Context derivation rules.
- Bounds and malformed-input behavior.
- How auto mode chooses or rejects context-v1.
- Benchmark command examples.
- Clear statement:
  - engineering measurement only
  - not CABAC
  - no H.265 superiority claim

TESTS:
Add/verify tests in libsrs_video:
- roundtrip all-zero block
- roundtrip single nonzero low-frequency coefficient
- roundtrip high-frequency sparse coefficients
- roundtrip mixed signs and magnitudes
- deterministic encode for same coefficients
- estimate bytes matches actual encoded length
- auto mode does not choose context when larger
- forced context mode reports failure cleanly when unsupported
- malformed: bad magic rejected
- malformed: unknown version rejected
- malformed: truncated header rejected
- malformed: truncated symbols rejected
- malformed: invalid context id rejected
- malformed: zero-run overflow rejected
- malformed: magnitude overflow rejected
- malformed: trailing garbage rejected
- old explicit/rANS residual streams still decode

BENCH TESTS:
- --compare-residual-context serializes all required report fields.
- no normal test requires FFmpeg.
- forced context failure row does not abort report.
- current default output bytes are unchanged when --compare-residual-context is not set.

ACCEPTANCE:
- New residual_context_entropy.rs is a complete module with rustdoc and security bounds.
- Context-v1 residual mode is benchmarkable.
- Default encode behavior remains unchanged unless auto proves context-v1 is smaller.
- Old FR2 streams still decode.
- Inter residual byte telemetry can show whether context-v1 helps.
- cargo fmt --all --check passes.
- cargo test -p libsrs_video residual_context passes.
- cargo test -p quality_metrics --bin bench_srsv2 residual_context passes.
- cargo clippy -p libsrs_video --all-targets -- -D warnings passes.
- cargo clippy -p quality_metrics --bin bench_srsv2 -- -D warnings passes.

RUN AFTER IMPLEMENTATION:
cargo fmt --all --check
cargo test -p libsrs_video residual_context
cargo test -p quality_metrics --bin bench_srsv2 residual_context
cargo clippy -p libsrs_video --all-targets -- -D warnings
cargo clippy -p quality_metrics --bin bench_srsv2 -- -D warnings

OPTIONAL FOLLOW-UP, NOT PART OF THIS BLOCK:
Re-run tools\windows_hevc_progress_baseline.ps1 and update docs/windows_hevc_progress_results.md with before/after numbers.
```
