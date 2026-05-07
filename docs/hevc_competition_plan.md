# SRSV2 vs HEVC-class codecs (engineering roadmap)

SRSV2 is an experimental native codec. **It does not beat H.265 / HEVC today**, and this document makes **no superiority claim** against H.265, x265, or any other standardized encoder.

Benchmarks and progress reports in this repository (**`bench_srsv2`**, Windows progress gates, JSON/Markdown summaries) are **engineering measurements only**: they record sizes, simple objective metrics, and reproducible commands. They are **not** a product scorecard and **not** proof of competitive ranking.

## Why HEVC-class (not AVC-only)

**H.264 / AVC** comparisons remain **useful**: FFmpeg **`libx264`** is widely available, fast to invoke, and helps validate plumbing, sanity-check metrics, and regress obvious encode/decode bugs. For **serious efficiency targets**, that baseline is **no longer enough**. Modern reference encoders and production expectations center on **H.265 / HEVC**-class toolsets (**`libx265`**, hardware HEVC, Main10 pipelines).

The **future primary external comparison target** for bitrate–quality discipline in this workspace is **x265 / libx265** (via FFmpeg when **`libx265`** is available), with **bitrate-matched** or tightly documented rate–distortion methodology. Until that path is implemented end-to-end, **`--compare-x264`** rows remain a **convenience**, not a substitute for HEVC-class benchmarking.

## HEVC-class engineering blockers (honest gap list)

These are **major** items typically associated with HEVC-class systems. Presence or absence here does **not** imply SRSV2 will eventually match any particular HEVC encoder; it only lists the class of work ahead.

1. **CTU-style 64×64 superblocks** — Coding tree units and hierarchy comparable to HEVC’s **64×64** CTU concept (not only fixed **16×16** macroblock grids).
2. **Bounded quadtree / recursive partitions** — Recursive splitting and signaling beyond today’s bounded **P**-frame partition experiments (**`FR2` rev 19+**), with byte-competitive **map** coding.
3. **Transform-size selection up to 32×32** — Large transforms (**16×16** and **32×32** classes) and associated mode signaling; today’s path tops out far below a full HEVC-style TU set.
4. **Context-adaptive entropy beyond StaticV1 / ContextV1** — **`ContextV1`** uses fixed per-context tables over a compact alphabet; it is **not** CABAC-class and **not** equivalent to mature adaptive binary arithmetic coding across coefficients and syntax as in HEVC.
5. **SAO-like sample adaptive offset** — Optional **sample adaptive offset**-style loop filtering **after** deblock (restoration pass); deblock alone is not a full HEVC in-loop filter stack.
6. **10-bit / HDR / Main10-style profile** — **8-bit 4:2:0** SDR is not sufficient for a **Main10**-class story; extended bit depth, range, and HDR metadata paths are out of scope for the current core unless explicitly scoped later.
7. **Tile / threaded 8K encode and decode** — Parallel **tiles**, wavefront-friendly scheduling, and encoder threading for very large pictures (**8K-class**) are not production-complete here.
8. **Bitrate-matched x265 comparison** — Defensible comparison requires **achieved bitrate** and quality on **both** sides (**two-pass**, constrained CRF sweeps, or a real matching loop—not CRF-only hand-waving). **`--match-x264-bitrate`**-style gaps apply equally to **x265** until implemented.
9. **Stronger RDO and mode decision** — Full **Lagrangian** mode decisions across partitions, references, transforms, and QP are production-grade in mature encoders; SRSV2’s RDO paths remain **bounded** and **heuristic**.
10. **Eventual GPU compute acceleration** — Competitive turnarounds at high resolution often assume **GPU**-friendly kernels (MC, transform, entropy, or full pipelines); the workspace does not yet provide that.

## Related docs

- Measurement practice and **`bench_srsv2`**: [`docs/srsv2_benchmarks.md`](srsv2_benchmarks.md)
- Motion / B-frame experimental status: [`docs/motion_search.md`](motion_search.md)
- Legacy AVC-oriented gap list (still accurate for **H.264-class** tools, not a substitute for the list above): [`docs/h264_competition_plan.md`](h264_competition_plan.md)

## Reporting rule

Any public summary must **not** state or imply that SRSV2 **beats** H.265, HEVC, x265, or **any** mature encoder **without** a documented, bitrate-matched methodology and independent verification.
