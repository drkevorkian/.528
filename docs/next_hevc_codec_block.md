# Next HEVC-Class Codec Implementation Block

**Source gate:** [`windows_hevc_progress_results.md`](windows_hevc_progress_results.md) (Windows HEVC progress gate + **`bench_srsv2 --compare-residual-contexts`** on the full corpus; engineering measurement only).

**Selected feature (exactly one):** **B. transform-size selection / coefficient layout improvements**

---

## Decision record (Block 6 rubric)

Evidence from gate run **2026-05-09** (corpus **64×64**, **8** frames, **QP 28**, seed **528**) and residual-context compare rows documented in the results file.

| Option | Choose if… | Verdict | Evidence from report |
|--------|------------|---------|---------------------|
| **A.** Context-adaptive residual training / richer coefficient contexts | Residual **ContextV1** helps (**total** or clearly isolated coefficient bytes down) | **No** | **`--compare-residual-contexts`**: **`context`** row **larger** total than **`off`** on **all** clips (**Δ** **+4113…+10108**). Caveat: rows mix **`raw`→`entropy`** inter + MV/partition stack with residual context—not a pure coefficient A/B. |
| **B.** Transform-size selection / coefficient layout | Residual ContextV1 **fails** byte objective | **Yes** | Totals **regress** everywhere in that compare; **residual** still **~82%** of **`SRSV2-pc-fixed16x16`** bytes on **`scene_cut`** (**4058** / **4949**). Next step attacks **coefficient packaging**, not MV entropy tables. |
| **C.** CTU64 encode path | Residual **no longer** dominates | **No** | Bottleneck table unchanged—**`residual`** still wins the partition-cost breakdown. |
| **D.** Quarter-pel luma | Prediction-error story dominates **and** MV tiny | **No** | **`MV/header`** **294** vs **`residual`** **4058** on the same reference row—MV is not the named blocker. |
| **E.** Bitrate-matched x265 sweep | Comparison **fairness** is the **primary** blocker | **Parallel** | Report still shows **large** bitrate mismatch vs optional x265 row (**relative gap ~0.475**). Run **E** for **measurement fairness**; **B** remains the **codec** implementation choice. |

**Conclusion:** Implement **B** next: improve **transform decision / coefficient layout** so residual energy is coded more efficiently **without** asserting **HEVC** parity or that SRSV2 “beats” **x265**.

---

## Numbers pinned to this decision

- **MV entropy ContextV1 vs StaticV1** (entropy-model compare): still **+1…+5** bytes on totals—no MV-context win.
- **Residual coefficient ContextV1** (`--compare-residual-contexts`): **total bytes up** on **`moving_square`**, **`scrolling_bars`**, **`checker`**, **`scene_cut`**; **PSNR-Y / SSIM-Y** matched at printed precision across paired rows.
- **Largest bottleneck row (unchanged reference):** `scene_cut` **`SRSV2-pc-fixed16x16`** → **`residual`** **4058** (**~82%** of **4949**).

---

## Cursor block (forward)

````text
BLOCK 7 GOAL:
Improve transform-size selection and/or coefficient layout so residual payload shrinks on fixed partitions without codec superiority claims.

SOURCE:
docs/windows_hevc_progress_results.md — selected feature B from measured gates.

WHY (data-backed):
- compare-residual-contexts totals regressed on every corpus clip (see results doc).
- Residual bucket still dominates SRSV2-pc-fixed16x16 (~82% on scene_cut).
- MV/header bucket remains secondary; QP motion ContextV1 already showed no total-byte win.

NON-GOALS:
- No claim that SRSV2 beats H.265/HEVC/x265.
- Do not conflate MV entropy ContextV1 with coefficient residual ContextV1 without holding inter syntax constant in benchmarks.

SUGGESTED WORK AREAS (pick minimal integrating path):
- libsrs_video SRSv2 transform decision surfaces tied to partition mode
- Coefficient scan/grouping and FR2 payload layout where experimental rev allows
- Bench: add/hold an apples-to-apples compare (fixed --inter-syntax entropy --entropy-model context --inter-partition fixed16x16) when isolating coefficient changes

MEASUREMENT:
- Re-run tools/windows_hevc_progress_baseline.ps1 and residual-context compares after changes
- Refresh docs/windows_hevc_progress_results.md

PARALLEL OPTIONAL:
- Bitrate-matched x265 tooling sweep (fairness only)
````

---

## Relation to option **E** (bitrate-matched x265)

Fair external comparison still benefits from **bitrate alignment**. That does **not** replace in-tree **transform/coefficient** work chosen here.
