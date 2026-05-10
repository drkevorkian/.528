# Next HEVC-Class Codec Implementation Block

**Source gate:** [`windows_hevc_progress_results.md`](windows_hevc_progress_results.md) — Windows HEVC progress baseline (`tools/windows_hevc_progress_baseline.ps1`) plus **`bench_srsv2 --compare-coeff-layouts`** on **`moving_square`**, **`scrolling_bars`**, **`checker`**, **`scene_cut`** (engineering measurement only).

**Selected feature (exactly one):** **B. transform-size decision improvements / new transform grouping**

---

## Decision record (Block 6 rubric)

Evidence from gate run **2026-05-10** (corpus **64×64**, **8** frames, **QP 28**, **keyint 8**, **motion-radius 4**, seed **528**, commit **3380d33**). CompactV1 rows: **`coeff_layout_compare_summary`** and `reports/<tag>/compare_coeff_layouts.json`.

| Option | Choose if… | Verdict | Evidence |
|--------|------------|---------|----------|
| **A.** Coefficient layout → **B** frames + variable partitions | CompactV1 **reduces total bytes** | **No** | Compact rows **larger** than legacy-zigzag on **all** clips: Δ total **+1556…+2828** B (`moving_square` **+2325**, `scrolling_bars` **+2828**, `checker` **+1556**, `scene_cut` **+2656**). |
| **B.** Transform-size decision / transform grouping | CompactV1 **fails** total-byte objective | **Yes** | Totals regress everywhere; intra telemetry shows **~54–56%** estimated packaging savings on **rev32** blocks but **does not** overcome full-bitstream cost here. |
| **C.** CTU64 encode path | Residual **no longer** dominates | **No** | **`scene_cut/SRSV2-pc-fixed16x16`**: **`residual`** **4058** / **4949** (**~82%**). |
| **D.** Quarter-pel luma | Prediction-error story dominates **and** MV tiny | **No** | Same reference row: **`MV/header`** **294** vs **`residual`** **4058**. |
| **E.** Bitrate-matched x265 sweep | Comparison **fairness** is primary | **Parallel** | Gate still reports large bitrate mismatch vs optional x265 reference (**relative gap ~0.475**). Does **not** replace **B**. |

**Conclusion:** Implement **B** next — improve **how transform size / grouping is chosen** so coded residual can shrink **without** claiming **HEVC/x265** superiority.

---

## Numbers pinned to this decision

| Clip | legacy-zigzag total | compact total (all scans tied) | Δ total | PSNR-Y / SSIM-Y (all rows) |
|------|--------------------:|-------------------------------:|--------:|----------------------------|
| `moving_square` | 6566 | 8891 | +2325 | unchanged |
| `scrolling_bars` | 8973 | 11801 | +2828 | unchanged |
| `checker` | 16761 | 18317 | +1556 | unchanged |
| `scene_cut` | 4949 | 7605 | +2656 | unchanged |

- **Scan modes:** **four-way tie** (zigzag = grouped-low-first = run-optimized = auto) on every clip for **total_bytes**.
- **Residual bottleneck (unchanged reference):** `scene_cut` **`SRSV2-pc-fixed16x16`** — **`residual`** **4058** (**~82%** of **4949**).

---

## Cursor block (forward)

````text
BLOCK 7 GOAL:
Improve transform-size decision and/or transform grouping so residual coding efficiency improves on fixed partitions — without H.265/x265 superiority claims.

SOURCE:
docs/windows_hevc_progress_results.md — Block 6 chose B after --compare-coeff-layouts showed larger totals for CompactV1 on every corpus clip.

WHY (data-backed):
- CompactV1 compare harness increased total bytes (+1556…+2828) on all four clips; PSNR/SSIM unchanged; scan modes tied.
- Residual bucket still ~82% on scene_cut/SRSV2-pc-fixed16x16.

NON-GOALS:
- No claim that SRSV2 beats H.265/HEVC/x265.
- Do not expand CompactV1 into B/variable partitions until total-byte wins are demonstrated (option A bar).

MEASUREMENT:
- Re-run tools/windows_hevc_progress_baseline.ps1 and bench_srsv2 --compare-coeff-layouts after changes
- Refresh docs/windows_hevc_progress_results.md

PARALLEL OPTIONAL:
- Bitrate-matched x265 sweep (fairness only; option E)
````

---

## Relation to option **E**

Fair external comparison still benefits from **bitrate alignment**; it does **not** replace in-tree **transform-size / grouping** work (**B**).
