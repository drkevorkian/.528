# Next codec implementation block (Block 6)

**Source gate:** [windows_h264_progress_results.md](windows_h264_progress_results.md) — chosen feature **C (partition syntax redesign)**.

**Problem statement (from gate):** Adaptive partition (`auto-fast` with RDO) uses **210 more** payload bytes than `fixed16×16` on `moving_square` (769 vs 559), and in **30** comparable sweep slices auto-fast **never** beat fixed16×16 on `total_bytes`. Partition map + MV/transform telemetry is small vs row total; the redesign must shrink **partition-related on-wire cost** and/or align encode decisions so adaptive partitions are byte-competitive.

---

## Cursor block — paste everything below into a new agent task

```
BLOCK 6 — Partition syntax v2 (full modules, no micro-edits)

GOAL
Implement “partition_syntax_v2” as a coherent encode/decode package so adaptive partitions can compete with fixed16×16 on bytes without abandoning FR2 streams that already exist.

CONTEXT
- Gate doc: docs/windows_h264_progress_results.md (choice C).
- Existing SRSv2 partition logic: crates/libsrs_video/src/srsv2/p_var_partition.rs and related inter MB wiring.
- Benchmark compare path: tools/quality_metrics bench_srsv2 already has --compare-partition-costs; extend with a clean compare flag for v1 vs v2 once both exist.

NON-NEGOTIABLES
1) Deliver FULL files/modules, not scattered one-line edits across ten call sites without structure.
2) Old FR2 bitstreams that decode today must still decode (backward compatible decode path OR gated experimental encode with explicit FR2 revision — pick one approach and document it in module rustdoc).
3) Security: validate all parse paths; fuzz-friendly bounds checks; no panics on malformed input in library code.
4) Every new module: top-level `//!` rustdoc describing wire format, invariants, and failure modes.
5) Tests: unit tests in libsrs_video + integration/syntax tests where parse/emit roundtrips matter.
6) Add a short human spec: docs/partition_syntax_v2.md (what bytes mean, version/rev, decode algorithm sketch).

CREATE / IMPLEMENT

A) crates/libsrs_video/src/srsv2/partition_syntax_v2.rs (new)
   - Types: partition map codebook or bit-packed representation, per-MB partition mode, optional “merge / split” flags.
   - API surface (suggested; adjust names to match repo style):
     - Encode: build map from encoder partition decisions → compact byte blob + side metadata needed by ME/residual stages.
     - Decode: parse blob → per-MB partition modes; return `Result`, detailed error enum.
   - Map compression:
     - Use run-length or prefix coding for flat regions (e.g. all 16×16) so typical clips pay ~0 extra vs implicit fixed grid when the encoder chooses global 16×16.
     - Explicitly optimize the moving_square class of patterns: mostly uniform partition with occasional splits — avoid per-PU fixed overhead when identical to neighbor.
   - MV sharing:
     - When sub-partitions share an MV (or share median predictor), emit ONE delta stream per shared group instead of duplicating per 8×8 leaf where legal by spec.
     - Document predictor selection rules so decoder reproduces encoder grouping.
   - Keep responsibilities separate: this module should not own ME; it owns serialization + canonical partition description consumed by ME/residual.

B) Wire integration
   - Add FR2 revision plan in docs/partition_syntax_v2.md:
     - Which revision bit activates v2 map on wire.
     - Exactly what changes vs current partition map bytes (field-bit layout or byte prelude).
   - Encoder: when setting enabled, emit v2 syntax; when disabled, preserve current behavior byte-for-byte for existing tests.
   - Decoder: branch on revision/feature flag; reject unknown map versions with structured error.

C) crates/libsrs_video/src/srsv2/partition_syntax_v2_tests.rs OR #[cfg(test)] mod tests inside partition_syntax_v2.rs
   - Roundtrip: random-but-valid partition maps → bytes → parse → assert equality.
   - Malformed: truncated buffer, impossible split pattern, MV-share group with missing leaf → expect errors, no panic.
   - Regression: “all 16×16” map compresses to expected small size (golden byte length or max-byte bound).

D) bench_srsv2 compare mode
   - New flag pair (names illustrative): --compare-partition-syntax {off|v1|v2|both} or --partition-syntax-v2-benchmark
   - Behavior: run the same synthetic corpus + QP/motion as --compare-partition-costs, add rows:
     - SRSV2-pc-fixed16x16 (unchanged)
     - SRSV2-pc-auto-fast-rdo with v1 map
     - SRSV2-pc-auto-fast-rdo with v2 map (when encoder flag wired)
   - JSON/MD reports must include total_bytes, partition_map_bytes (telemetry), and label distinguishing v1 vs v2.

ACCEPTANCE METRICS (must report in bench output after wiring)
1) On tools/windows_h264_progress_baseline.ps1 corpus (moving_square): auto-fast RDO total_bytes with v2 map is NOT worse than v1 by more than 5% OR beats fixed16×16 on bytes in ≥1 comparable sweep slice — whichever is achieved first, document which gate passed with numbers.
2) partition_map_bytes + MV-related bytes (existing bench fields) must not regress vs v1 when the encoder chooses fixed16×16 (should match or improve).
3) cargo test -p libsrs_video --no-fail-fast passes; cargo clippy -p libsrs_video --all-targets -- -D warnings passes.

DELIVERABLES CHECKLIST
- [ ] partition_syntax_v2.rs (+ any small adjacent types file ONLY if strictly needed)
- [ ] docs/partition_syntax_v2.md
- [ ] Wire into srsv2 mod (pub use / module decl) and encoder/decoder call sites in focused commits mentally grouped as “integration”.
- [ ] Tests + bench compare flag
- [ ] No claim in comments that SRSV2 beats H.264; measurement-only wording.

Start by reading p_var_partition.rs and the current partition map emit/parse sites, then implement v2 as a replacement layer rather than bolting ad-hoc bit flags into ten files.
```

---

## Local notes (for humans)

- This block intentionally asks for **new files** and a **spec doc** so the next agent does not scatter conditionals without a named abstraction.
- If FR2 revision bump is politically heavy, the implementation agent may use an **experimental encode setting** that is off by default but still ships decode for the new syntax—either way, `docs/partition_syntax_v2.md` must state the rule.
- Re-run `tools\windows_h264_progress_baseline.ps1` after implementation and update `docs/windows_h264_progress_results.md` only when comparing before/after numbers (optional follow-up, not part of this file’s minimum).
