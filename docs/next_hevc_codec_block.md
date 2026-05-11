# Next HEVC-Class Codec Implementation Block

**Source gate:** [`windows_hevc_progress_results.md`](windows_hevc_progress_results.md) — Windows HEVC progress corpus plus **`bench_srsv2 --compare-transform-grouping`** on **`moving_square`**, **`scrolling_bars`**, **`checker`**, **`scene_cut`** (64×64, 8 frames, QP **28**, keyint **8**, motion-radius **4**, seed **528**, git **7ed0cba**). Engineering measurement only.

**Selected feature (exactly one):** **Motion- and prediction-aware Auto transform grouping** — steer **`AutoByResidual`** / **`RdoFast`** so **`legacy8x8`-sized totals** are recovered on **motion-heavy** clips (**`scrolling_bars`**, **`scene_cut`**) **without** sacrificing the **`four4x4`** / **`auto-rdo-fast`** byte wins already seen on **`checker`** and **`moving_square`**.

---

## Decision record (Block 4 rubric)

Evidence from **`--compare-transform-grouping`** (same gate settings as [`windows_hevc_progress_results.md`](windows_hevc_progress_results.md) § Transform grouping). Primary metric: **`total_bytes`**. Telemetry **`residual_bytes`** is **not** comparable row-to-row without reading the fairness note in the results doc.

| Clip | `legacy8x8` total | Best total among tested modes | Which mode | Δ vs legacy (best − legacy) |
|------|------------------:|------------------------------:|------------|---------------------------:|
| `moving_square` | 8984 | 8176 | **`four4x4`** | **−808** |
| `scrolling_bars` | 11894 | 11894 | **`legacy8x8`** | **0** |
| `checker` | 18410 | 13838 | **`four4x4`** | **−4572** |
| `scene_cut` | 7698 | 7698 | **`legacy8x8`** | **0** |

**Verdict**

- **Localized / high-contrast residual clips** (`moving_square`, `checker`): **explicit `four4x4`** or **`auto-rdo-fast`** can **beat** **`legacy8x8`** on **total bytes** (with **different** PSNR-Y / SSIM-Y vs legacy at printed precision).
- **Smooth motion / scene-cut style clips** (`scrolling_bars`, `scene_cut`): **`legacy8x8`** remains the **smallest total**; **`four4x4`**, **`auto-residual-aware`**, and **`auto-rdo-fast`** **regress** totals in this run.

**Conclusion:** The next focused block is **not** “always Four4×4” nor “revert grouping experiments”; it is **conditional Auto grouping** that **tracks prediction effectiveness** (temporal smoothness, MV magnitude, residual energy distribution) so **motion-heavy** clips **collapse toward Tx8×8-style totals** while **keeping** wins where **Four4×4** pays off.

---

## Numbers pinned to this decision

| Clip | L8 total | F4 total | auto-RA total | auto-RDO total | PSNR-Y (L8/F4/ARA/ARDO) | SSIM-Y (L8/F4/ARA/ARDO) |
|------|---------:|---------:|--------------:|---------------:|-------------------------|-------------------------|
| `moving_square` | 8984 | 8176 | 8662 | 8276 | 14.895 / 14.987 / 14.897 / 14.962 | 0.619 / 0.653 / 0.622 / 0.645 |
| `scrolling_bars` | 11894 | 13586 | 12724 | 12752 | 14.655 / 14.679 / 14.667 / 14.755 | 0.558 / 0.582 / 0.558 / 0.576 |
| `checker` | 18410 | 13838 | 18954 | 14100 | 10.506 / 10.624 / 10.506 / 10.502 | 0.131 / 0.130 / 0.131 / 0.133 |
| `scene_cut` | 7698 | 8942 | 8072 | 8362 | 13.300 / 13.404 / 13.300 / 13.412 | 0.694 / 0.714 / 0.694 / 0.710 |

Artifacts: `var/bench/windows_hevc_progress/reports/<tag>/compare_transform_grouping.{json,md}`.

---

## Cursor block (forward)

````text
BLOCK 8 GOAL:
Make Auto transform grouping (ResidualAware + RdoFast) clip-aware so motion-smooth / prediction-friendly
MBs avoid Four4×4 side-cost regressions (scrolling_bars, scene_cut) while preserving wins on checker / moving_square.

SOURCE:
docs/windows_hevc_progress_results.md — § Transform grouping (bench_srsv2 --compare-transform-grouping), git 7ed0cba.

WHY (data-backed):
- legacy8x8 wins total bytes on scrolling_bars and scene_cut; four4x4 wins on moving_square and checker.
- auto-rdo-fast is strictly between: better than legacy on 2 clips, worse on 2 clips.

NON-GOALS:
- No H.265/HEVC/x265 superiority claims.
- Do not expand variable partitions or B-picture coeff layout until fixed-grid totals are stable.

MEASUREMENT:
- Re-run bench_srsv2 --compare-transform-grouping on the four corpus clips (same WxH, frames, QP 28, keyint 8, motion-radius 4, seed 528).
- Refresh docs/windows_hevc_progress_results.md and this file.

OPTIONAL HYGIENE (parallel):
- Align residual_bytes telemetry across grouping rows so byte budgets are auditable without relying on total_bytes alone.
````

---

## Relation to earlier option **E**

Bitrate-matched external references remain **parallel** fairness work; they **do not** replace **in-tree grouping policy** tuning above.
