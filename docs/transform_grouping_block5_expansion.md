# Transform grouping — Block 5 expansion (post–Block 4 gate)

This note applies **only** when the Block 4 gate passes:

- **Gate:** transform grouping must reduce **residual** bytes vs **`legacy8x8`** on **at least two** of the corpus clips **moving-square**, **scrolling-bars**, **checker**, and **scene-cut**, **without** lowering **PSNR-Y** or **SSIM-Y** vs legacy on those qualifying rows (see `var/bench/windows_hevc_progress/` reports and `docs/windows_hevc_progress_results.md`).

If the gate **fails**, do **not** expand transform grouping along these lines; prioritize **residual token redesign** instead.

## Variable-partition harness (implemented)

`bench_srsv2` exposes **`--compare-transform-grouping-with-partitions`** (alias `compare_transform_grouping_with_partitions`):

- Runs the same four-row compare as **`--compare-transform-grouping`** under **`fixed16x16`**, then repeats under **`split8x8`** (`--inter-partition split8x8`).
- JSON fields: **`compare_transform_grouping`**, **`transform_grouping_compare_summary`**, plus **`compare_transform_grouping_split8x8`**, **`transform_grouping_compare_summary_split8x8`**.
- Markdown emits a second section for the **`split8×8`** partition block.
- **`--bframes 0`** only; mutually exclusive with **`--compare-transform-grouping`**.

Use this mode to regression-check grouping savings when inter macroblocks use **variable** partitions instead of fixed **16×16**.

## B-frame transform grouping — revision plan

Transform grouping telemetry and RDO paths are validated primarily on **I/P** (`--bframes 0`). Extending grouping decisions to **B** frames needs an explicit plan:

1. **Wire semantics:** Decide whether B uses the same **`transform_grouping_mode`** / **`transform_decision_mode`** as P or separate toggles; document FR2 payload implications if B differs.
2. **Anchor context:** Ensure grouping cost estimates use the correct reference(s) (backward / forward / weighted) per B macroblock so **`residual_bytes`** and **`transform_grouping_*`** telemetry stay comparable to P.
3. **Benchmark:** Add a **`--compare-transform-grouping`** variant allowed with **`--bframes 1`** (or a dedicated flag) once normalization no longer forces **no-B**; extend JSON/Markdown with a clear label when B is enabled.
4. **Gate:** Re-run the corpus gate with **B enabled** before claiming parity with the Block 4 P-only result.

## Further variable-partition integration

Beyond **`split8x8`**, future work:

- Hook the compare harness into additional **`--inter-partition`** modes the encoder supports once they are stable in compact inter syntax.
- Optionally emit **per-partition** transform-grouping counters (when partition maps vary inside the CTU) so regressions are attributable to grouping vs partition choice.

## References

- Encoder/bench implementation: `tools/quality_metrics/src/bin/bench_srsv2.rs`
- Related roadmap context: `docs/next_hevc_codec_block.md`
