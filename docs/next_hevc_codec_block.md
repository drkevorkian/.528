# Next HEVC-Class Codec Implementation Block

**Source gate:** [`windows_hevc_progress_results.md`](windows_hevc_progress_results.md) — § **Residual TokenV2** (`bench_srsv2 --compare-residual-token-v2`) on **`moving_square`**, **`scrolling_bars`**, **`checker`**, **`scene_cut`** (64×64, 8 frames, QP **28**, keyint **8**, motion-radius **4**, seed **528**, git **223a9f8**). Engineering measurement only; **no** claim that SRSV2 beats H.265/HEVC.

**Selected feature (exactly one):** **Transform math / quantization redesign** (coefficient domain: scale, deadzone, and/or transform pipeline) — **not** expanding TokenV2 into **B** frames or variable partitions until residual tails shrink on the fixed-grid harness.

---

## Decision record (Block 6 rubric)

Primary metric: **`total_bytes`** per compare row (`row.bytes` / `details.total_bytes` in JSON). **`token-v2`** vs **`legacy`**:

| Clip | legacy total | token-v2 total | Δ (v2 − legacy) | PSNR-Y change | SSIM-Y change |
|------|-------------:|---------------:|------------------:|---------------|---------------|
| `moving_square` | 6566 | 14963 | **+8397** | none (match) | none (match) |
| `scrolling_bars` | 8973 | 19590 | **+10617** | none (match) | none (match) |
| `checker` | 16761 | 42125 | **+25364** | none (match) | none (match) |
| `scene_cut` | 4949 | 11205 | **+6256** | none (match) | none (match) |

**Verdict**

- **TokenV2 does not “help” on clip totals here** — every **`token-v2`** row is **larger** than **`legacy`**.
- **Quality (PSNR-Y / SSIM-Y)** is **unchanged** at the precision stored in the JSON for these pairs — the regression is **size**, not measured displayed-frame quality on this gate.
- **Smallest total-byte regression** vs legacy: **`scene_cut`**; **largest**: **`checker`**.

**Paths not chosen (same rubric)**

- **Integrate TokenV2 into B / variable partitions:** deferred — TokenV2 **failed** the total-byte gate on **all** fixed-grid clips.
- **CTU64 encode path:** deferred — residual / coefficient tails still dominate legacy telemetry on the reference **`scene_cut`** row in [`windows_hevc_progress_results.md`](windows_hevc_progress_results.md) § Biggest Byte Bottleneck; nothing in Block 6 suggests MV/header became the cap.
- **Quarter-pel luma:** deferred — no evidence prediction error (vs side information) became the primary limiter in this compare.

**Conclusion:** Next engineering focus is **coefficient budget**: quantization / transform behavior so fewer bits are spent for the same QP and measured quality, **before** widening TokenV2 surface area.

---

## Artifacts

- `var/bench/windows_hevc_progress/reports/<tag>/compare_residual_token_v2.{json,md}`
- Command lines: `var/bench/windows_hevc_progress/commands_run.txt` (Block 6 append)

---

## Cursor block (forward)

````text
BLOCK 7 GOAL:
Reduce coefficient bits at fixed QP without sacrificing displayed-frame PSNR-Y / SSIM-Y on the
four-corpus gate (64×64, 8 frames, keyint 8, motion-radius 4, seed 528) — transform / quantization
path (not TokenV2 wiring into B or var-partitions).

SOURCE:
docs/windows_hevc_progress_results.md — § Residual TokenV2 (bench_srsv2 --compare-residual-token-v2), git 223a9f8.

WHY (data-backed):
- token-v2 totals exceed legacy on every clip (+6256 … +25364 B).
- Simulated AC subset on moving_square: v2 packed bytes ≥ v1 rANS blob sum in JSON summary.

NON-GOALS:
- No H.265/HEVC/x265 superiority claims.
- Do not expand TokenV2 into B-picture or adaptive partition paths until fixed-grid totals improve.

MEASUREMENT:
- Re-run bench_srsv2 --compare-residual-token-v2 after changes (same corpus, QP, keyint, motion-radius).
- Refresh docs/windows_hevc_progress_results.md § Residual TokenV2 and this file.
````

---

## Relation to earlier option **E**

Bitrate-matched external references remain **parallel** fairness work; they **do not** replace in-tree quantization / transform work above.
