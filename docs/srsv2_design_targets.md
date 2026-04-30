# SRSV2 design targets and roadmap

SRSV2 is the **modern native `.528` video codec** for this project. It is **not** a small replacement for the legacy grayscale prototype alone: the engineering target is a **credible 8K-first** codec that scales down to **1080p / 1440p / 4K**, remains **decode-friendly and parallel**, and can extend to **above-8K** with explicitly higher memory use and latency.

This document is **normative for intent** and **descriptive for current code**. Implementation catches up incrementally; nothing here guarantees parity with H.264/HEVC/AV1/VVC until backed by **measured** comparisons (see [Benchmarks and competitive claims](#benchmarks-and-competitive-claims)).

## Strategic positioning

- **Aspiration:** SRSV2 should **eventually** compete with **H.264 / AVC** on **bitrate–distortion** at HD through **8K**, with room to chase stronger codecs at extreme presets.
- **Honesty:** The repository **does not** claim “better than H.264” in README or marketing copy until **benchmark evidence** exists (`docs/srsv2_benchmarks.md`).
- **Interoperability:** SRSV2 is **not** bitstream-compatible with MPEG codecs; comparisons are always **encode/decode to raw/YUV** plus metric tooling.

## Resolution and scalability

| Tier | Typical resolutions | Notes |
|------|---------------------|--------|
| Downscale | 1080p, 1440p | Fast decode, Baseline-oriented tooling |
| Standard | 4K | Main profile, balanced presets |
| Primary target | **8K** | Decode parallelism and tiling are first-class design constraints |
| Extended | **Above 8K** | **Research** profile; slower encode/decode, higher RAM; optional tooling |

Decode paths must remain **efficient and parallel** (tile/thread boundaries, bounded allocations). Encode may be **much slower** in high-quality modes when compression gains justify it.

**Do not** optimize the codebase **only** for tiny synthetic frames; conformance tests may use small dimensions, but limits and data structures must assume **large pictures**.

## Codec profiles (`SrsVideoProfile`, sequence header byte 16)

Wire values are stable — extend only by adding new bytes with decoder support.

| Value | Profile | Role |
|-------|---------|------|
| 0 | **Baseline** | 1080p / 1440p / mobile-class; **fast decode** priority |
| 1 | **Main** | **4K / 8K** general playback and production (default today for many helpers) |
| 2 | **Pro** | Creator / editing / archival; **4:2:2** and **4:4:4** readiness (formats signaled elsewhere) |
| 3 | **Lossless** | Near-lossless / archival emphasis |
| 4 | **Screen** | Game / desktop / text / **AI-generated** content; screen-specific tools |
| 5 | **Ultra** | **8K** high-quality compression; slower encode acceptable |
| 6 | **Research** | Above-8K and experimental features; not general interchange default |

## Encoder presets (policy)

Presets are **product knobs**, not yet fully wired end-to-end everywhere:

| Preset | Goal vs H.264 (when measured) |
|--------|-------------------------------|
| **Fast** | May **lose** to H.264 at equal bitrate — acceptable for realtime-ish paths |
| **Balanced** | **Target:** match or beat H.264 on BD-rate style curves |
| **High** | **Target:** clearly **beat** H.264 at comparable subjective quality |
| **Insane** | Slow; chase **AV1 / VVC-class** efficiency where feasible (years-long roadmap) |

## Technical roadmap (vs H.264-class competence)

Ordered roughly by dependency; many items overlap across releases.

1. **P-frames** — in progress / prototype (`FR2` rev 2 path).
2. **B-frames** or **alternate references** — temporal layering later.
3. **Tiled 8K decode** — parallelism and cache locality.
4. **64×64 and 128×128 superblocks** — hierarchical coding units.
5. **Adaptive block splitting** — alongside transforms/QP.
6. **Integer-pel motion search first** — already directionally aligned with current P prototype.
7. **Half / quarter-pel** refinement — after stable integer MV.
8. **Transform + quantization tuning** — perceptual Q, RDO hooks.
9. **Adaptive entropy coding** — beyond fixed tuple planes where gains justify complexity.
10. **Deblock** and **directional / CDEF-class** filtering — in-loop tools.
11. **Rate control** — VBR/CBR, HRD-friendly pacing for containers.
12. **10-bit + HDR metadata** — PQ/HLG signaling aligned with sequence/header contracts.
13. **Screen-content mode** — palette / skip / IBCCC-class shortcuts under **Screen** profile.
14. **Quality metrics** — **PSNR**, **SSIM / MS-SSIM**, optional **VMAF** when FFmpeg or Netflix VMAF is available in CI/dev machines.

## Benchmarks and competitive claims

**Acceptance rule:** Do **not** publish “SRSV2 beats H.264” (or similar) without:

- **Controlled comparisons** using **`ffmpeg`** (or equivalent) for **H.264** encode/decode baselines when available.
- Reported: **bitrate**, **compression ratio**, **PSNR**, **SSIM / MS-SSIM**, optional **VMAF**, **encode FPS**, **decode FPS**, resolution, preset, and hardware notes.

See **`docs/srsv2_benchmarks.md`** for the proposed harness shape and reporting template.

## Relationship to `docs/srsv2_codec.md`

`srsv2_codec.md` summarizes **what exists in tree today**. This document anchors **long-term** intent so profiles and limits evolve without surprise.
