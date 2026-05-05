# Next Cursor Block: Partition Syntax Redesign

## BLOCK 7 GOAL

Redesign SRSV2 variable-partition syntax so `auto-fast-rdo` can stop losing bytes to `fixed16x16` on small/global-motion clips while preserving old FR2 decode paths.

Gate evidence:

- Windows progress gate chose **C. Partition syntax redesign**.
- `auto-fast-rdo` did not beat `fixed16x16` on the best partition-cost row: **781 bytes vs 559 bytes** on `moving_square`.
- Auto-fast had **0 smaller-byte wins** across **30** comparable quality/bitrate sweep slices.
- Progress summary bottleneck was `poor_prediction_proxy`: **681 / 781 bytes** on the `SRSV2-pc-auto-fast-rdo` row, indicating partitioned syntax/overhead is not accounted for well enough before adding new prediction tools.

## DO NOT

- Do not change existing FR2 decode behavior for revisions already shipped in this branch.
- Do not remove legacy partition syntax.
- Do not add quarter-pel, residual entropy redesign, or intra prediction work in this block.
- Do not make `auto-fast` default unless tests and benchmark evidence justify it.
- Do not claim SRSV2 beats H.264.

## WORK IN COMPLETE MODULES

Primary codec modules:

- `crates/libsrs_video/src/srsv2/partition_syntax.rs` (new)
- `crates/libsrs_video/src/srsv2/p_var_partition.rs`
- `crates/libsrs_video/src/srsv2/inter_mv.rs`
- `crates/libsrs_video/src/srsv2/rdo.rs`
- `crates/libsrs_video/src/srsv2/mod.rs`

Benchmark/report modules:

- `tools/quality_metrics/src/bin/bench_srsv2.rs`
- `tools/windows_h264_progress_baseline.ps1`
- `docs/srsv2_benchmarks.md`

## REQUIRED FEATURE AREAS

### 1. Partition Syntax Redesign Module

Create `partition_syntax.rs` to own all variable-partition map and syntax-byte logic. It should expose a compact, testable API instead of scattering syntax decisions through `p_var_partition.rs`.

Required public types:

- `PartitionSyntaxMode`
- `PartitionMapCodec`
- `PartitionMapStats`
- `PartitionSyntaxStats`
- `PartitionSyntaxDecision`

Required public functions:

- `encode_partition_map_v2(...)`
- `decode_partition_map_v2(...)`
- `estimate_partition_map_v2_bytes(...)`
- `choose_partition_map_codec(...)`
- `estimate_partition_syntax_overhead(...)`
- `validate_partition_map_v2(...)`

Required map codecs:

- raw one-byte-per-MB fallback
- RLE by partition tag
- row-run RLE for repeated rows
- all-fixed16x16 sentinel

Design requirements:

- Deterministic output for the same partition map.
- Explicit byte counts for map header, map body, transform syntax, and per-partition syntax.
- Hostile input bounds: decoded run lengths must not exceed macroblock count; row-run count must not exceed row count; unknown tags must fail.
- No allocation proportional to untrusted declared sizes unless already range-checked.

### 2. Map Compression

Replace the current partition-map write path with `PartitionMapCodec` selection.

Rules:

- Always compute raw, RLE, row-run, and all-fixed candidates.
- Choose the smallest valid map encoding.
- Tie-break deterministically: all-fixed, row-run, RLE, raw.
- Expose chosen codec and byte counts in `SrsV2PartitionEncodeStats`.

Required stats:

- `partition_map_codec_raw_count`
- `partition_map_codec_rle_count`
- `partition_map_codec_row_run_count`
- `partition_map_codec_all_fixed_count`
- `partition_map_header_bytes`
- `partition_map_body_bytes`
- `partition_syntax_overhead_bytes`

### 3. MV Sharing

Reduce redundant MV bytes when variable partitions split a macroblock but share the same or predictable motion.

Required behavior:

- For split/rect partitions, detect when child partition MVs are identical or predictably equal to the parent 16x16 MV.
- Encode a compact shared-MV marker where legal in the new syntax.
- Preserve old MV stream decode for old FR2 revisions.
- Add byte estimates for shared-MV candidates to RDO.

Required types/functions:

- `PartitionMvSharingMode`
- `PartitionMvSharingStats`
- `estimate_partition_mv_sharing_bytes(...)`
- `choose_partition_mv_sharing(...)`
- `encode_partition_mv_sharing_v2(...)`
- `decode_partition_mv_sharing_v2(...)`

Safety:

- Shared-MV decode must validate partition type before applying sharing markers.
- Any marker referring to a missing parent or unsupported partition layout must fail.
- Shared MVs must still pass existing MV range and half-grid validation where applicable.

### 4. Partition RDO Rewrite

Rewrite partition RDO candidate construction so byte cost is complete and centralized.

Required changes:

- Candidate scoring must use `RdoCost` via `score_candidate`.
- Candidate cost must include:
  - partition map bytes
  - partition syntax/header bytes
  - transform ID bytes
  - MV bytes after sharing/compression
  - residual bytes
  - block AQ bytes
  - skip flags
- Remove or isolate any SAD-only shortcut that can select split partitions without pricing syntax.
- Keep `fixed16x16` as a candidate in all auto modes.

Required tests:

- flat clip candidates choose fixed16x16 when split side-info dominates.
- global-pan candidates choose fixed16x16 or shared-MV partition when split residual gain is insufficient.
- mixed-motion synthetic macroblock can choose split8x8 when residual gain pays.
- RDO tie-break is deterministic.
- candidate count cap is enforced.

### 5. FR2 Revision Plan

Add exactly one new experimental FR2 revision only if required by the redesigned wire syntax.

If a new revision is needed:

- Use the next available revision after currently handled SRSV2 revisions.
- Update payload classification.
- Add explicit decode errors for malformed new syntax.
- Keep all older FR2 revision decoders unchanged.
- Document the revision in `docs/srsv2_codec.md` and `docs/srsv2_benchmarks.md`.

If no new revision is needed:

- Document why the existing revision can carry the redesigned map codec safely.
- Prove old streams still decode with existing roundtrip tests.

### 6. Benchmark Compare Mode

Add a benchmark compare mode focused on partition syntax redesign:

Required CLI flag:

- `bench_srsv2 --compare-partition-syntax`

Rows:

- `SRSV2-partition-legacy`
- `SRSV2-partition-map-rle`
- `SRSV2-partition-map-row-run`
- `SRSV2-partition-shared-mv`
- `SRSV2-partition-syntax-v2`

Report fields:

- total bytes
- PSNR-Y
- SSIM-Y
- partition map codec counts
- partition map header/body bytes
- partition syntax overhead bytes
- MV bytes
- residual bytes
- transform syntax bytes
- RDO candidates tested
- partitions rejected by RDO
- fixed16x16 vs auto-fast byte delta

Markdown must include a short engineering-only verdict. No H.264 superiority language.

### 7. Windows Progress Script Update

Update `tools/windows_h264_progress_baseline.ps1` to run `--compare-partition-syntax` for each synthetic clip once the mode exists.

The progress summary should include partition syntax rows only as engineering evidence. It must still run without FFmpeg.

## TESTS

Codec unit tests:

- raw partition map v2 roundtrip
- RLE partition map v2 roundtrip
- row-run partition map v2 roundtrip
- all-fixed sentinel roundtrip
- malformed partition map run overflow fails
- unknown partition tag fails
- row-run height overflow fails
- shared-MV marker roundtrip
- shared-MV marker rejects invalid partition layouts
- old FR2 variable-partition streams still decode

RDO tests:

- flat/global motion rejects expensive split
- mixed motion accepts split when residual savings pay
- shared-MV candidate beats duplicate-MV split when equal quality
- deterministic tie-break
- candidate cap enforced

Benchmark tests:

- `--compare-partition-syntax` serializes all rows
- missing/failed row is reported without aborting unrelated rows where possible
- Markdown includes partition syntax telemetry
- progress script still succeeds without FFmpeg

## ACCEPTANCE

- `auto-fast-rdo` no longer explodes bytes on flat/global clips in the Windows progress corpus.
- `--compare-partition-syntax` shows whether the redesign beats legacy partition syntax.
- `fixed16x16` remains available and old streams still decode.
- Partition syntax byte costs are visible in JSON and Markdown.
- No tiny scattered edits: partition map syntax, MV sharing, RDO selection, and benchmark reporting are implemented as complete modules/features.

## VERIFICATION COMMANDS

Run:

```powershell
cargo fmt --all --check
cargo test -p libsrs_video --no-fail-fast
cargo test -p quality_metrics --no-fail-fast
cargo clippy -p libsrs_video -p quality_metrics --all-targets -- -D warnings
powershell -ExecutionPolicy Bypass -File tools\windows_h264_progress_baseline.ps1
```

Then confirm:

- `var/bench/windows_h264_progress/summary.json` exists.
- `var/bench/windows_h264_progress/summary.md` exists.
- partition syntax compare JSON/Markdown exists for each Windows progress clip.
- `docs/windows_h264_progress_results.md` can be refreshed with the new partition syntax evidence.
