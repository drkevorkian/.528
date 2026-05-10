# Next HEVC-Class Codec Implementation Block

**Source gate:** [`windows_hevc_progress_results.md`](windows_hevc_progress_results.md) — Windows HEVC progress baseline (`tools/windows_hevc_progress_baseline.ps1`) plus **`bench_srsv2 --compare-coeff-layouts`** on **`moving_square`**, **`scrolling_bars`**, **`checker`**, **`scene_cut`** (engineering measurement only).

**Selected feature (exactly one):** **B. Transform-size decision / coefficient grouping**

---

## Decision record (Block 6 rubric)

Evidence from gate run **2026-05-10** (corpus **64×64**, **8** frames, **QP 28**, **keyint 8**, **motion-radius 4**, seed **528**, git **c4a2203**). CompactV1 rows: **`coeff_layout_compare_summary`** and `reports/<tag>/compare_coeff_layouts.json`.

| Option | Choose if… | Verdict | Evidence |
|--------|------------|---------|----------|
| **A.** Integrate coefficient layout into B pictures + variable partitions | CompactV1 **reduces total bytes** | **No** | Compact-zigzag **larger** than legacy-zigzag on **all** clips: Δ total **+1649…+2921** B (`moving_square` **+2418**, `scrolling_bars` **+2921**, `checker` **+1649**, `scene_cut` **+2749**). |
| **B.** Transform decision / coefficient grouping | CompactV1 **fails** total-byte objective | **Yes** | Totals regress everywhere; encoder **`coeff_layout_savings_percent`** vs legacy estimate is **~1.4–39.7%** by clip on compact rows but **does not** reduce **full** bitstream size here. |
| **C.** CTU64 encode path | Residual **no longer** dominates | **No** | **`scene_cut/SRSV2-pc-fixed16x16`**: **`residual`** **4058** / **4949** (**~82%**). |
| **D.** Quarter-pel luma | Prediction-error story dominates **and** MV tiny | **No** | Same reference row: **`MV/header`** **294** vs **`residual`** **4058**. |
| **E.** Bitrate-matched x265 sweep | Comparison **fairness** is primary | **Parallel** | Gate still reports large bitrate mismatch vs optional x265 reference (**relative gap ~0.475**). Does **not** replace **B**. |

**Conclusion:** Implement **B** next — improve **how transform size / coefficient grouping is chosen** so coded residual can shrink **without** claiming **HEVC/H.265/x265** superiority.

---

## Numbers pinned to this decision

| Clip | legacy-zigzag total | compact-zigzag total | Δ total | PSNR-Y / SSIM-Y (all five rows) |
|------|--------------------:|---------------------:|--------:|----------------------------------|
| `moving_square` | 6566 | 8984 | +2418 | unchanged |
| `scrolling_bars` | 8973 | 11894 | +2921 | unchanged |
| `checker` | 16761 | 18410 | +1649 | unchanged |
| `scene_cut` | 4949 | 7698 | +2749 | unchanged |

- **Scan modes:** **four-way tie** (zigzag = grouped-low-first = run-optimized = auto) on every clip for **`total_bytes`** / **`residual_bytes`**.
- **Telemetry:** **`residual_bytes_delta_vs_legacy_zigzag`** negative on every compact row (e.g. **`moving_square`** **−5028** B) — useful for regression tracking, **not** a substitute for total-byte wins.
- **Residual bottleneck (unchanged reference):** `scene_cut` **`SRSV2-pc-fixed16x16`** — **`residual`** **4058** (**~82%** of **4949**).

---

## Cursor block (forward)

````text
BLOCK 7 GOAL:
Improve transform-size decision and/or coefficient grouping so residual coding efficiency improves on fixed partitions — without H.265/x265 superiority claims.

SOURCE:
docs/windows_hevc_progress_results.md — Block 6 chose B after --compare-coeff-layouts showed larger totals for CompactV1 on every corpus clip (git c4a2203).

WHY (data-backed):
- CompactV1 compare harness increased total bytes (+1649…+2921) on all four clips; PSNR/SSIM unchanged; scan modes tied; telemetry residual deltas negative but totals grew.
- Residual bucket still ~82% on scene_cut/SRSV2-pc-fixed16x16.

NON-GOALS:
- No claim that SRSV2 beats H.265/HEVC/x265.
- Do not expand CompactV1 into B pictures / variable partitions until total-byte wins are demonstrated (option A bar).

MEASUREMENT:
- Re-run tools/windows_hevc_progress_baseline.ps1 and bench_srsv2 --compare-coeff-layouts after changes
- Refresh docs/windows_hevc_progress_results.md

PARALLEL OPTIONAL:
- Bitrate-matched x265 sweep (fairness only; option E)
````

---

## Relation to option **E**

Fair external comparison still benefits from **bitrate alignment**; it does **not** replace in-tree **transform-size / grouping** work (**B**).
