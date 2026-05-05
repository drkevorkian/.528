# Next codec move (data-driven)

This document records **one** follow-on engineering direction (**option A–E** below) using artifacts from this workspace run. It complements [`docs/srsv2_design_targets.md`](srsv2_design_targets.md) and [`docs/srsv2_benchmarks.md`](srsv2_benchmarks.md). **No competitive superiority claims.**

## Chosen option

**C — Partition syntax / partition-side-information path**

Improve wiring and byte accounting for **variable partitions** and **auto-fast** so sweep rows can reflect real wins vs **fixed16×16**, rather than only SAD-driven splits.

## Evidence

Sources read on disk:

- [`var/bench/srsv2_h264_progress_summary.md`](../var/bench/srsv2_h264_progress_summary.md) (generated via `bench_srsv2 --h264-progress-summary` using Gentoo baseline JSON from **`gentoo_checker_64x64`**).
- [`var/bench/gentoo_baseline/SUMMARY.md`](../var/bench/gentoo_baseline/SUMMARY.md) and [`docs/gentoo_baseline_results.md`](gentoo_baseline_results.md) (six-clip baseline index).
- Progress summary JSON/Markdown fields:
  - **§3 Auto-fast vs fixed16×16:** “In **30** comparable sweep slices, auto-fast **never** beat fixed16×16 on **total_bytes** (ties possible).” — directly matches option **C** “choose if” gate (auto-fast vs fixed16×16 struggle).
  - **§2 RDO:** `SRSV2-pc-auto-fast-rdo` vs sad-only bytes **18139** vs **18141**; `partition_rejected_by_rdo` **7** — RDO is active but sweep still shows no slice-level win for auto-fast vs fixed16×16.
  - **§1 ContextV1 vs StaticV1 (checker clip):** Static **6574** bytes vs Context **6572** bytes — **2** byte delta on MV entropy compare; **not** the primary bottleneck for partition strategy.
  - **`next_bottleneck`:** **`poor_prediction_proxy`** (~**94.5%** share on the auto-fast RDO detail row) — large **unbucketed** remainder indicates instrumentation/accounting gaps alongside partition telemetry (supports prioritizing **partition / side-info** clarity before chasing MV entropy alone).

## Rejected options

| Option | Why not chosen (from these artifacts) |
|--------|--------------------------------------|
| **A** — Quarter-pel luma | MV/header **share ~2.8%** on the progress byte snapshot; not the dominant labeled bucket vs **poor_prediction_proxy**. |
| **B** — Context-adaptive **residual** entropy | Labeled **`inter_residual`** share **0** in the snapshot row (telemetry/residual accounting artifact); sweep gate for auto-fast vs fixed16×16 already fails — residual entropy alone does not address that signal. |
| **D** — Better **intra** | No artifact here shows **I-frame / keyint** cost dominating the progress answers; focus data is **inter** sweep + partition compares. |
| **E** — Bitrate-matched **x264** methodology | Progress inputs omitted optional **compare-x264** bench JSON (“skipped”); comparison methodology is unfinished but **sweep §3** already gives a clearer partition-focused gate (**C**). |

## Exact next Cursor block

**Block 6 — Partition pathway follow-through:** extend partition-map / transform tagging estimates in benchmarks and encoder telemetry so **`poor_prediction_proxy`** shrinks and **auto-fast vs fixed16×16** sweep rows become interpretable; optional **compare-x264** JSON path for a future methodology pass (**E**). **No new FR2 revision** unless a proven wire gap appears.

## Acceptance metric (verifiable)

On **`bench_srsv2 --sweep-quality-bitrate`** with the same documented sweep caps as [`docs/gentoo_baseline_results.md`](gentoo_baseline_results.md): **≥1** comparable sweep slice where **`inter_partition=auto-fast`** has **strictly lower** **`total_bytes`** than **`fixed16×16`** at the same **`(inter_syntax, entropy_model, partition_cost_model, qp)`** key **or**, alternatively, **partition_map_bytes + transform_syntax_bytes** (from **`compare_partition_costs`** JSON `details.partition`) drop **≥10%** on **`gentoo_checker_64x64`** at baseline QP/keyint vs the archived Gentoo JSON — without breaking decode of existing streams (**`cargo test --workspace`** green).

---

## Reference checklist (how we got here)

| Step | Tool / artifact |
|------|------------------|
| Host baseline | `bash tools/gentoo_dev_check.sh`, optional `bash tools/gentoo_bench_baseline.sh` |
| SRSV2 compares | `bench_srsv2` `--compare-inter-syntax`, `--compare-rdo`, `--compare-partition-costs`, `--compare-entropy-models`, `--sweep-quality-bitrate` |
| Aggregated answers | `bench_srsv2 --h264-progress-summary --progress-entropy-json … --progress-partition-costs-json … --progress-sweep-json …` (encode args still required by CLI — use any valid YUV + report paths, or a repo corpus clip under `var/bench/gentoo_baseline/corpus/`) |

### Principles

1. **Default stays conservative:** [`SrsV2EntropyModelMode::StaticV1`](../crates/libsrs_video/src/srsv2/rate_control.rs) remains default; **`ContextV1`** stays experimental until measurements justify otherwise ([`video_bitstream_v2.md`](video_bitstream_v2.md)).
2. **No external superiority claims:** Optional **x264** rows are reference telemetry only.
3. **Bounded decode:** MV ContextV1 decode budgets remain mandatory ([`context_inter_entropy.rs`](../crates/libsrs_video/src/srsv2/context_inter_entropy.rs)).

## Further references

- FR2 revision map: [`video_bitstream_v2.md`](video_bitstream_v2.md)
- Benchmark ethics and flags: [`srsv2_benchmarks.md`](srsv2_benchmarks.md)
- Encode/decode targets: [`srsv2_design_targets.md`](srsv2_design_targets.md)
- Gentoo environment: [`gentoo_dev_environment.md`](gentoo_dev_environment.md)
