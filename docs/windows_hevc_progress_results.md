# Windows HEVC Progress Gate Results

_Engineering measurement only. This report does **not** claim SRSV2 beats H.265/HEVC, x265, or any mature encoder._

## Run

- Date: 2026-05-10 13:43:30 -06:00 (baseline); **`--compare-coeff-layouts`** finished ~13:44 -06:00; **`--compare-transform-grouping`** (Block 4) recorded **2026-05-08** with git **7ed0cba**
- Output root: `var\bench\windows_hevc_progress\`
- Seed: 528; fps: 30; QP: 28
- Corpus: `moving-square`, `scrolling-bars`, `checker`, `scene-cut` (64×64, 8 frames)
- Gate knobs (Block 4): **keyint 8**, **motion-radius 4**, **`--residual-entropy auto`**, harness normalizes to **compact** inter + **fixed16×16** + **coeff-layout compact** / zigzag (see `bench_srsv2` `--compare-transform-grouping`).
- FFmpeg available: **True**; libx264: **True**; libx265: **True**
- Commands: `var\bench\windows_hevc_progress\commands_run.txt` (coeff-layout lines from earlier baseline + Block 4 gen/bench lines)
- Git (coeff-layout JSON): **c4a2203**; Git (transform-grouping JSON): **7ed0cba**
- Residual-context tables: `reports\<tag>\compare_residual_contexts.{json,md}`
- Coefficient-layout compare: `reports\<tag>\compare_coeff_layouts.{json,md}` (`bench_srsv2 --compare-coeff-layouts`, same WxH×frames / QP **28** / keyint **8** / motion-radius **4** as gate)
- Transform-grouping compare: `reports\<tag>\compare_transform_grouping.{json,md}` (`bench_srsv2 --compare-transform-grouping`)

## Required Results

- Best StaticV1 row: clip=scene_cut, label=SRSV2-entropy-StaticV1, mode=static, bytes=4956, PSNR-Y=13.3004, SSIM-Y=0.6935
- Best ContextV1 row: clip=scene_cut, label=SRSV2-entropy-ContextV1, mode=context, bytes=4961, PSNR-Y=13.3004, SSIM-Y=0.6935
- Best partition syntax v1 row: clip=scene_cut, label=fixed16x16, mode=v1, bytes=4949, PSNR-Y=13.3004, SSIM-Y=0.6935
- Best partition syntax v2 row: clip=moving_square, label=auto-fast-rdo-v2, mode=v2, bytes=8523, PSNR-Y=15.7806, SSIM-Y=0.7528
- Best sweep row: clip=moving_square, label=row_19, mode=entropy/static/rdo-fast/auto-fast, bytes=8360, PSNR-Y=15.7482, SSIM-Y=0.7538
- Optional x265 result: status=ok, bytes=3561, bitrate=106830, PSNR-Y=100, SSIM-Y=1

## Questions Answered

### Transform grouping (`bench_srsv2 --compare-transform-grouping`)

Rows (fixed order): **`legacy8x8`**, **`four4x4`** (residual-aware decision), **`auto-residual-aware`**, **`auto-rdo-fast`** (`--rdo fast`). **Total bytes** = full SRSV2 clip payload; **`residual_bytes`** = encoder telemetry bucket (see note).

**Fairness note:** On **`legacy8x8`**, telemetry **`residual_bytes`** counts compact rev33 **P** residual bytes as in earlier gates. On **mixed transform-grouping** rows, the same field is often reported as **0** in this harness while **`transform_grouping_bytes`** carries rev34/rev35 grouping + coefficient packaging — **do not** interpret **`residual_bytes`** alone as comparable across rows; use **`total_bytes`** for clip size.

| Clip | legacy8x8 total / `residual_bytes` | four4x4 total / `residual_bytes` | auto-residual-aware total / `residual_bytes` | auto-rdo-fast total / `residual_bytes` | PSNR-Y (L8 / F4 / ARA / ARDO) | SSIM-Y (L8 / F4 / ARA / ARDO) | Winner (min **total**) |
| --- | ---: | ---: | ---: | ---: | --- | --- | --- |
| `moving_square` | 8984 / 1223 | 8176 / 0 | 8662 / 0 | 8276 / 0 | 14.895 / 14.987 / 14.897 / 14.962 | 0.6194 / 0.6526 / 0.6218 / 0.6454 | **`four4x4`** (8176 B) |
| `scrolling_bars` | 11894 / 1225 | 13586 / 0 | 12724 / 0 | 12752 / 0 | 14.655 / 14.679 / 14.667 / 14.755 | 0.5576 / 0.5820 / 0.5581 / 0.5762 | **`legacy8x8`** (11894 B) |
| `checker` | 18410 / 1283 | 13838 / 0 | 18954 / 0 | 14100 / 0 | 10.506 / 10.624 / 10.506 / 10.502 | 0.1312 / 0.1299 / 0.1312 / 0.1328 | **`four4x4`** (13838 B) |
| `scene_cut` | 7698 / 1193 | 8942 / 0 | 8072 / 0 | 8362 / 0 | 13.300 / 13.404 / 13.300 / 13.412 | 0.6935 / 0.7141 / 0.6935 / 0.7099 | **`legacy8x8`** (7698 B) |

**Summaries from JSON (`transform_grouping_compare_summary`):**

- `moving_square`: four4x4 **−808** B vs legacy; auto-residual-aware **−322** B; auto-rdo-fast **−708** B (all vs **8984** B baseline).
- `scrolling_bars`: four4x4 **+1692** B; auto-residual-aware **+830** B; auto-rdo-fast **+858** B (vs **11894** B).
- `checker`: four4x4 **−4572** B; auto-residual-aware **+544** B; auto-rdo-fast **−4310** B (vs **18410** B).
- `scene_cut`: four4x4 **+1244** B; auto-residual-aware **+374** B; auto-rdo-fast **+664** B (vs **7698** B).

**Direct answers**

1. **Did `four4x4` always win on total bytes?** **No** — it wins on **`moving_square`** and **`checker`**, loses on **`scrolling_bars`** and **`scene_cut`** vs **`legacy8x8`**.
2. **Did `auto-rdo-fast` beat legacy everywhere?** **No** — smaller totals than legacy on **`moving_square`** and **`checker`**, larger on **`scrolling_bars`** and **`scene_cut`**.
3. **Quality vs legacy at printed precision?** **PSNR-Y / SSIM-Y change by row** (see table); not identical across modes.

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

| Clip | legacy-zigzag total B | legacy-zigzag `residual_bytes` | compact-zigzag total B | compact-zigzag `residual_bytes` | Δ total (compact−legacy) | PSNR-Y | SSIM-Y |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `moving_square` | 6566 | 6251 | 8984 | 1223 | **+2418** | 14.8952 | 0.619397 |
| `scrolling_bars` | 8973 | 8658 | 11894 | 1225 | **+2921** | 14.6554 | 0.557608 |
| `checker` | 16761 | 16446 | 18410 | 1283 | **+1649** | 10.5063 | 0.131160 |
| `scene_cut` | 4949 | 4634 | 7698 | 1193 | **+2749** | 13.3004 | 0.693524 |

On every clip, **zigzag**, **grouped-low-first**, **run-optimized**, and **auto** compact rows reported **identical** `total_bytes` / `residual_bytes` (table shows **compact-zigzag** as representative). Encoder telemetry on compact rows (**`coeff_layout_savings_percent`** vs legacy coefficient-byte **estimate**): **`moving_square`** ≈ **31.3%**, **`scrolling_bars`** ≈ **28.3%**, **`checker`** ≈ **1.4%**, **`scene_cut`** ≈ **39.7%** — **full clip totals still grew**, so estimated packaging savings did not translate into smaller bitstreams here.

**Coefficient-layout compare — direct answers**

1. **Did CompactV1 reduce residual bytes (telemetry)?** The reported **`residual_bytes`** field **drops** on compact rows while **`residual_bytes_delta_vs_legacy_zigzag`** is **negative** on every clip — this reflects **mixed FR2 rev32/33 vs legacy** accounting in the bench metric, **not** proof that underlying prediction residual energy shrank.
2. **Did CompactV1 reduce total bytes?** **No.** Δ total **+1649…+2921** on **every** clip (table above).
3. **Did PSNR/SSIM stay the same or improve?** **Same** at the precision shown — all five rows match per clip.
4. **Which scan won most often?** **None — four-way tie** on **`total_bytes`** / **`residual_bytes`** for every corpus clip in this run.
5. **Is residual still the largest bottleneck?** **Yes** on the same partition-cost reference row: **`scene_cut/SRSV2-pc-fixed16x16`** → **`residual`** **4058** / **4949** (**~82%**).

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

See **`docs/next_hevc_codec_block.md`** — Block 4 transform-grouping numbers narrow the next execution target (**motion-heavy clips still favor `legacy8x8` on totals**).

Reason (historical): **`--compare-coeff-layouts`** showed CompactV1 **increases** total bytes on **all four** clips; **`--compare-transform-grouping`** (Block 4) shows **mixed** totals vs **`legacy8x8`** (**engineering measurement only**).

Allowed planning labels: **A** CTU64 encode path; **B** transform-size / coefficient layout; **C** context-adaptive residual training (only if residual ContextV1 wins totals); **D** quarter-pel luma motion; **E** bitrate-matched x265 sweep (fairness).

## Notes

- `--compare-x265` is optional and skipped when FFmpeg/libx265 is unavailable.
- `--compare-x264-and-x265` runs only when both encoders are reported by FFmpeg.
- x265 rows are CRF-style reference rows; they are **not** bitrate-matched proof.
- Full machine-readable summary: `var\bench\windows_hevc_progress\summary.json`.
