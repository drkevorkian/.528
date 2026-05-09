# Next HEVC-Class Codec Implementation Block

**Source gate:** [`windows_hevc_progress_results.md`](windows_hevc_progress_results.md) (Windows HEVC progress gate; engineering measurement only).

**Selected feature (exactly one):** **C. context-adaptive residual coefficient entropy**

---

## Decision record (A–G rubric)

Evidence is taken from the published gate doc (run **2026-05-08**, corpus **64×64**, **8** frames, **QP 28**).

| Option | Choose if… | Verdict | Evidence from report |
|--------|----------------|---------|---------------------|
| **A. CTU64 encode path** | Partition Syntax V2 reduced overhead **and** fixed16×16 limits quality/bytes | **No** | V2 **did** shrink **map** bytes (−112 on several pairs) but **AutoFast RDO total bytes did not improve** (Δ total +64…+212 per clip). **AutoFast RDO did not beat fixed16×16 on total bytes anywhere** (Δ +1781…+9445). CTU geometry is reporting-only today; **encode path** is premature vs residual dominance. |
| **B. Quadtree partitioning** | CTU geometry exists **and** partition syntax overhead is under control | **No** | CTU stats exist for bench telemetry only (not encode). **partition syntax bucket = 0** bytes on bottleneck row `scene_cut/SRSV2-pc-fixed16x16`. Overhead is **not** the named blocker. |
| **C. Residual context entropy** | **Residual bytes dominate** reports | **Yes** | **Biggest byte bottleneck:** **`residual`** **4058** bytes of **4949** total (**~82%**) on `scene_cut/SRSV2-pc-fixed16x16`. **`MV/header`** only **294** (**~5.9%**). |
| **D. Quarter-pel luma motion** | Prediction error dominates **and** MV/header bytes under control | **No** | Residual dominates; MV/header is small vs residual. No gate metric showing prediction-error-led SSIM failure as primary blocker. |
| **E. SAO-like restoration** | Blocking/ringing hurts SSIM after deblock | **No** | Gate does not isolate post-deblock artifact SSIM; byte story points to **coefficients**, not loop-filter gaps. |
| **F. 10-bit/HDR/Main10** | Platform roadmap > current byte wins | **No** | Gate is 8-bit corpus-focused; bottleneck table does not argue HDR first. |
| **G. Bitrate-matched x265 sweep** | Comparison **fairness** is the **blocker** | **Partial / parallel** | Report states SRSV2 vs x265 ** bitrate similarity: no** (relative gap **0.475**); recommends **bitrate-matched sweep for fairness**. That is **measurement/tooling** (`--match-x265-bitrate` etc.), **not** a substitute for lowering **dominant residual bytes** inside SRSV2. **Next codec implementation block** remains **C**; **G** stays a **benchmark/adoption** track, not the single codec feature chosen here. |

**Conclusion:** Implement **C** next to attack the **largest named byte bucket** (**inter/residual**). MV/context entropy (StaticV1 vs ContextV1) already showed **no** total-byte win on this gate; **coefficient** residual entropy is the aligned follow-on.

---

## Gate numbers (verbatim anchors)

- Bottleneck row: `scene_cut/SRSV2-pc-fixed16x16`, **total_bytes=4949**.
- Buckets: **`residual` 4058** (winner); **`MV/header` 294**; **`intra/keyframe cost` 597**; **`partition syntax` 0**.
- Partition Syntax V2: map savings **−112** but AutoFast RDO **total** often **worse** than v1 on this gate.
- ContextV1 vs StaticV1: **+1…+5** bytes (no reduction).
- AutoFast RDO vs fixed16×16: **larger** total bytes on every clip in the table.
- Optional x265 row: **not** bitrate-matched to SRSV2 (report: use bitrate-matched methodology for fairness).

---

## Cursor Block

````text
BLOCK 6 GOAL:
Implement context-adaptive residual coefficient entropy as the next HEVC-class architecture layer.

SOURCE:
docs/windows_hevc_progress_results.md — selected exactly one feature:
C. context-adaptive residual coefficient entropy

WHY (report-backed):
- Largest named byte bucket on bottleneck row scene_cut/SRSV2-pc-fixed16x16: residual 4058 / 4949 total (~82%).
- MV/header 294 bytes (~5.9%) — not the dominant bucket.
- Partition syntax 0 bytes on that breakdown — not the next squeeze target.
- StaticV1 vs ContextV1 did not reduce total bytes; residual coefficient coding is the orthogonal next step.
- Do not claim SRSV2 beats H.265/HEVC/x265.

WORK IN COMPLETE MODULES:
- crates/libsrs_video/src/srsv2/residual_context_entropy.rs  (NEW — primary)
- crates/libsrs_video/src/srsv2/residual_entropy.rs
- crates/libsrs_video/src/srsv2/frame_codec.rs
- crates/libsrs_video/src/srsv2/p_frame_codec.rs
- crates/libsrs_video/src/srsv2/rate_control.rs
- crates/libsrs_video/src/srsv2/mod.rs
- tools/quality_metrics/src/bin/bench_srsv2.rs
- docs/residual_context_entropy.md

NON-NEGOTIABLES:
1. Security first: decoders reject malformed streams with structured errors; no panics on hostile input.
2. Backward compatibility: existing residual modes and all prior FR2 revisions still decode unchanged.
3. Default encoder behavior unchanged unless auto mode proves context-v1 smaller per block (same rule family as rANS vs explicit).
4. No scattered drive-by refactors — introduce the module and wire through explicit integration points.
5. No H.265 superiority claims in code comments, docs, or bench output.
6. Bitrate-matched x265 (option G) may proceed in parallel as tooling; this block is in-tree residual entropy only.

CREATE COMPLETE MODULE:
crates/libsrs_video/src/srsv2/residual_context_entropy.rs

MODULE MUST CONTAIN:
- rustdoc explaining:
  - coefficient residual entropy (not MV entropy)
  - experimental; not CABAC; not HEVC parity claim
  - malformed-input policy
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
- Coefficient contexts (example buckets — finalize in doc):
  - plane: Y / U / V
  - frequency bucket: DC-ish / mid / high
  - zero-run context
  - sign context
  - magnitude bucket context
- Bounds:
  - max symbols per block; max bytes per block/frame
  - max zero-run length
  - checked length prefixes and symbol counts
- Determinism: same coefficients + same metadata → identical codewords

WIRE / FORMAT PLAN:
- Document experimental residual-context blob in docs/residual_context_entropy.md (e.g. magic S2RC, version 1).
- New FR2 revision only after explicit plan in doc; existing revisions byte-compatible.

RATE CONTROL / SETTINGS:
- Extend residual mode surface: add context / context-v1 alongside explicit, auto, rans.
- Default remains auto.
- Auto must not pick context-v1 when larger than explicit/rANS for that block.

ENCODER INTEGRATION:
- Intra + P paths: keep explicit + rANS; add context-v1 candidate; auto picks smallest.
- Telemetry: residual_context_blocks, bytes, symbols, zero_runs, fallback_blocks, failure_reason.

DECODER INTEGRATION:
- Branch for new gated payload only; reject bad magic/version/truncation/overflow/garbage per structured errors.

BENCHMARK:
tools/quality_metrics/src/bin/bench_srsv2.rs

ADD FLAG:
- --compare-residual-context

ROWS (example labels):
- SRSV2-residual-auto-current
- SRSV2-residual-context-v1-forced
- SRSV2-residual-auto-with-context-candidate

REPORT FIELDS (minimum):
- residual_context_status
- residual_context_blocks / bytes / symbols / zero_runs / fallback_blocks
- residual_context_failure_reason
- inter_residual_bytes (and intra if surfaced)
- total_bytes, PSNR-Y, SSIM-Y, encode/decode FPS

DOCS:
docs/residual_context_entropy.md — purpose, wire format, bounds, benchmark examples, no-superiority language.

TESTS (libsrs_video):
- roundtrip zeros, sparse HF, mixed signs; deterministic encode
- estimate matches length; auto never chooses larger context-v1
- malformed streams rejected (magic, version, truncation, context id, zero-run, magnitude, garbage)
- legacy explicit/rANS streams still decode

BENCH TESTS:
- compare mode serializes required fields; default run unchanged when flag off; no FFmpeg in normal tests

ACCEPTANCE:
- residual_context_entropy.rs complete with rustdoc + bounds
- Benchmark flag works; old streams decode
- cargo fmt --all --check
- cargo test -p libsrs_video (residual_context filters as applicable)
- cargo test -p quality_metrics --bin bench_srsv2 (residual_context filters)
- cargo clippy -p libsrs_video --all-targets -- -D warnings
- cargo clippy -p quality_metrics --bin bench_srsv2 -- -D warnings

OPTIONAL FOLLOW-UP (NOT THIS BLOCK):
Re-run tools/windows_hevc_progress_baseline.ps1 and refresh docs/windows_hevc_progress_results.md after residual-context lands.
````

---

## Relation to option **G** (bitrate-matched x265)

Fair external comparison is **blocked** without bitrate alignment (see gate). The workspace may already expose **`--match-x265-bitrate`** / sweep tooling; **that track does not replace** shrinking **residual** bytes inside SRSV2. Treat **G** as **benchmark fairness**, **C** as **codec**.
