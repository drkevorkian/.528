# Windows HEVC Progress Gate Results

_Engineering measurement only. This report does **not** claim SRSV2 beats H.265/HEVC, x265, or any mature encoder._

## Run

- Date: 2026-05-10 10:16:21 -06:00
- Output root: `var\bench\windows_hevc_progress\`
- Seed: 528; fps: 30; QP: 28
- Corpus: `moving-square`, `scrolling-bars`, `checker`, `scene-cut` (64x64, 8 frames)
- FFmpeg available: **True**; libx264: **True**; libx265: **True**
- Commands: `var\bench\windows_hevc_progress\commands_run.txt` (includes `--compare-residual-contexts` per clip; **post-gate** `--compare-coeff-layouts` lines appended)
- Residual-context tables: `reports\<tag>\compare_residual_contexts.{json,md}`
- Coefficient-layout compare: `reports\<tag>\compare_coeff_layouts.{json,md}` (`bench_srsv2 --compare-coeff-layouts`, same WxHxframes/QP/keyint/motion as gate)

## Required Results

- Best StaticV1 row: clip=scene_cut, label=SRSV2-entropy-StaticV1, mode=static, bytes=4956, PSNR-Y=13.3004, SSIM-Y=0.6935
- Best ContextV1 row: clip=scene_cut, label=SRSV2-entropy-ContextV1, mode=context, bytes=4961, PSNR-Y=13.3004, SSIM-Y=0.6935
- Best partition syntax v1 row: clip=scene_cut, label=fixed16x16, mode=v1, bytes=4949, PSNR-Y=13.3004, SSIM-Y=0.6935
- Best partition syntax v2 row: clip=moving_square, label=auto-fast-rdo-v2, mode=v2, bytes=8523, PSNR-Y=15.7806, SSIM-Y=0.7528
- Best sweep row: clip=moving_square, label=row_19, mode=entropy/static/rdo-fast/auto-fast, bytes=8360, PSNR-Y=15.7482, SSIM-Y=0.7538
- Optional x265 result: status=ok, bytes=3561, bitrate=106830, PSNR-Y=100, SSIM-Y=1

## Questions Answered

### Did Partition Syntax V2 reduce adaptive partition overhead?

**Partially: V2 reduced split8x8 total bytes and map bytes, but AutoFast RDO total bytes did not improve in this gate.**

| Clip | Pair | v1 total bytes | v2 total bytes | Δ total (v2-v1) | v1 map bytes | v2 map bytes | Δ map (v2-v1) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `moving_square` | `auto-fast-rdo` | 8347 | 8523 | 176 | 112 | 0 | -112 |
| `moving_square` | `split8x8` | 8806 | 8792 | -14 | 112 | 0 | -112 |
| `scrolling_bars` | `auto-fast-rdo` | 18418 | 18630 | 212 | 112 | 0 | -112 |
| `scrolling_bars` | `split8x8` | 18936 | 18922 | -14 | 112 | 0 | -112 |
| `checker` | `auto-fast-rdo` | 19489 | 19659 | 170 | 112 | 0 | -112 |
| `checker` | `split8x8` | 20043 | 20029 | -14 | 112 | 0 | -112 |
| `scene_cut` | `auto-fast-rdo` | 12229 | 12293 | 64 | 112 | 0 | -112 |
| `scene_cut` | `split8x8` | 12829 | 12815 | -14 | 112 | 0 | -112 |

### Did ContextV1 reduce bytes vs StaticV1?

**No: ContextV1 did not reduce total bytes vs StaticV1 in this gate.**

| Clip | StaticV1 bytes | ContextV1 bytes | Δ context-static |
| --- | ---: | ---: | ---: |
| `moving_square` | 6591 | 6593 | 2 |
| `scrolling_bars` | 8975 | 8976 | 1 |
| `checker` | 16791 | 16792 | 1 |
| `scene_cut` | 4956 | 4961 | 5 |

### Did residual coefficient ContextV1 (`bench_srsv2 --compare-residual-contexts`) help?

**No on total compressed bytes:** the `context` row exceeded `off` on **every** clip. PSNR-Y / SSIM-Y matched at printed precision between paired rows.

**Fairness note:** `off` rows use default **raw** inter syntax; `context` rows upgrade predicted **P** frames to **entropy** inter + **context** MV + **fixed16x16** (required for FR2 residual ContextV1). Totals **mix** syntax changes with residual-context mode—not an isolated coefficient experiment.

| Clip | Row | Inter syntax | Total bytes | Telemetry `residual_bytes` | PSNR-Y | SSIM-Y |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| `moving_square` | off | raw | 6783 | 6251 | 14.8952 | 0.619397 |
| `moving_square` | context | entropy | 11264 | 1632 | 14.8952 | 0.619397 |
| `scrolling_bars` | off | raw | 9190 | 8658 | 14.6554 | 0.557608 |
| `scrolling_bars` | context | entropy | 14685 | 1632 | 14.6554 | 0.557608 |
| `checker` | off | raw | 16978 | 16446 | 10.5063 | 0.13116 |
| `checker` | context | entropy | 27086 | 1632 | 10.5063 | 0.13116 |
| `scene_cut` | off | raw | 5166 | 4634 | 13.3004 | 0.693524 |
| `scene_cut` | context | entropy | 9279 | 1632 | 13.3004 | 0.693524 |

- Largest delta total (context minus off): **`checker`** (**+10108** bytes).

- Bottleneck row again (partition-cost reference): `scene_cut/SRSV2-pc-fixed16x16` — winner bucket **`residual`** **4058** / **4949** total (**0.82** share).

**Residual-context compare — direct answers**

1. **Did residual ContextV1 reduce residual bytes?** Telemetry **`residual_bytes`** is **lower** on every **`context`** row here, but that field mixes intra payloads + **P** residual telemetry and the **`context`** row uses different FR2/inter paths—not an isolated proof ContextV1 compressed coefficients more tightly.
2. **Did it reduce total bytes?** **No.** Delta (**`context`** minus **`off`**): `moving_square` **+4481**, `scrolling_bars` **+5495**, `checker` **+10108**, `scene_cut` **+4113**.
3. **Did it preserve PSNR/SSIM?** **Yes** at the precision printed in the table (paired rows match).
4. **Which clip improved most (total bytes)?** **None** — every **`context`** total was **larger**.
5. **Which clip regressed most (total bytes)?** **`checker`** (**+10108** bytes).
6. **Is residual still the largest bottleneck?** **Yes** on `scene_cut/SRSV2-pc-fixed16x16`: **`residual`** **4058** / **4949** (**~82%**).

### Coefficient layout CompactV1 (`bench_srsv2 --compare-coeff-layouts`)

Harness holds **`--residual-entropy auto`**, **`--residual-context off`**, **`--inter-partition fixed16x16`**, **`--block-aq off`**, and upgrades **`--inter-syntax raw`→`compact`** so **FR2 rev33** fixed-grid **P** coefficients are valid. Row order: **legacy-zigzag**, then four **compact** scans (zigzag / grouped-low-first / run-optimized / auto).

| Clip | legacy-zigzag total B | legacy-zigzag `residual_bytes` | compact total B (all scans tied) | compact `residual_bytes` | Δ total (compact−legacy) | PSNR-Y | SSIM-Y |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `moving_square` | 6566 | 6251 | 8891 | 1130 | **+2325** | 14.8952 | 0.619397 |
| `scrolling_bars` | 8973 | 8658 | 11801 | 1132 | **+2828** | 14.6554 | 0.557608 |
| `checker` | 16761 | 16446 | 18317 | 1190 | **+1556** | 10.5063 | 0.131160 |
| `scene_cut` | 4949 | 4634 | 7605 | 1100 | **+2656** | 13.3004 | 0.693524 |

Intra **CompactV1** telemetry on compact rows reports **`coeff_layout_savings_percent`** ≈ **53.6–56.4%** vs legacy estimate on the **rev32** intra plane packaging — but **full clip totals still grew**, so packaging savings did not overcome other bytes on these clips.

**Coefficient-layout compare — direct answers**

1. **Did CompactV1 reduce residual bytes (telemetry)?** The printed **`residual_bytes`** field **drops** on compact rows because **FR2 rev32/33** paths change how intra/**P** coefficient volume is attributed vs legacy intra + **P** chunks — **not** a strict apples-to-apples “same buckets” comparison.
2. **Did CompactV1 reduce total bytes?** **No.** Δ total **+1556…+2828** on **every** clip (table above).
3. **Did it preserve PSNR/SSIM?** **Yes** — all five rows match per clip at shown precision.
4. **Which scan mode won most often?** **None — four-way tie:** zigzag, grouped-low-first, run-optimized, and auto produced **identical** `total_bytes` / `residual_bytes` on **all** clips in this gate.
5. **Is residual still the biggest bottleneck?** **Yes** on the same reference partition-cost row: **`scene_cut/SRSV2-pc-fixed16x16`** → **`residual`** **4058** / **4949** (**~82%**).

### Did AutoFast RDO beat fixed16x16 anywhere?

**No: AutoFast RDO did not beat fixed16x16 on total bytes in this gate.**

| Clip | fixed16x16 bytes | AutoFast RDO bytes | Δ auto-fixed |
| --- | ---: | ---: | ---: |
| `moving_square` | 6566 | 8347 | 1781 |
| `scrolling_bars` | 8973 | 18418 | 9445 |
| `checker` | 16761 | 19489 | 2728 |
| `scene_cut` | 4949 | 12229 | 7280 |

### Did B-half or B-weighted become useful?

**No: B-half and B-weighted did not beat B-int in this gate.**

## B-Mode Results

| Clip | P-only bytes | B-int bytes | B-half bytes | B-weighted bytes |
| --- | ---: | ---: | ---: | ---: |
| `moving_square` | 6783 | 6500 | 6955 | 6900 |
| `scrolling_bars` | 9190 | 9938 | 10247 | 10303 |
| `checker` | 16978 | 12382 | 13067 | 12766 |
| `scene_cut` | 5166 | 5418 | 5778 | 5802 |

Counts: B-half smaller than B-int on **0** clips; B-weighted smaller than B-int on **0** clips.

### Did SRSV2 approach x265 at similar bitrate/quality?

**No: achieved bitrates are not similar (relative gap 0.475); use a bitrate-matched x265 sweep for fairness.**

- SRSV2 bitrate (optional x265 clip): 203490
- x265 bitrate: 106830
- Similar bitrate (<=10% gap): **no**

## Biggest Byte Bottleneck

Source row: `scene_cut/SRSV2-pc-fixed16x16`; total bytes: **4949**.

| Bucket | Bytes | Share |
| --- | ---: | ---: |
| `MV/header` | 294 | 0.0594 |
| `residual` | 4058 | 0.82 |
| `partition syntax` | 0 | 0 |
| `transform syntax` | 0 | 0 |
| `intra/keyframe cost` | 597 | 0.1206 |
| `poor prediction` | 0 | 0 |

Winner: **`residual`** with **4058** bytes.

## Next Feature

Exactly one next feature: **B. transform-size decision improvements / new transform grouping**.

Reason from this gate: **`--compare-coeff-layouts`** shows **CompactV1** scan/layout variants **increase total bytes on every corpus clip** while **PSNR-Y / SSIM-Y** are unchanged; **scan modes tied**. Residual still **~82%** of **`scene_cut/SRSV2-pc-fixed16x16`** payload. Per Block 6 rubric: **do not** choose **A** (expand CompactV1 to B/variable partitions) when CompactV1 fails the **total-byte** objective here; prioritize **transform-size / grouping** next. **Not C/D** (residual still dominates; MV/header secondary). **E** remains optional for fairness only.

Allowed planning labels (Block 6 rubric): **A** integrate coefficient layout into B frames + variable partitions (if CompactV1 helps totals); **B** transform-size decision / new transform grouping (if CompactV1 fails); **C** CTU64 encode path (if residual no longer dominates); **D** quarter-pel luma (if prediction error dominates); **E** bitrate-matched x265 sweep (if comparison fairness dominates).

## Notes

- `--compare-x265` is optional and skipped when FFmpeg/libx265 is unavailable.
- `--compare-x264-and-x265` runs only when both encoders are reported by FFmpeg.
- x265 rows are CRF-style reference rows; they are **not** bitrate-matched proof.
- Full machine-readable summary: `var\bench\windows_hevc_progress\summary.json`.
