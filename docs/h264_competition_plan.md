# SRSV2 vs mature codecs (engineering roadmap — **not** a superiority claim)

SRSV2 is an experimental native codec. Maturing codecs such as **H.264** combine decades of standardized tools. This note lists **real blockers** before SRSV2 could be argued seriously on efficiency at matched complexity constraints — **without** claiming SRSV2 “beats” H.264 today.

1. **B-frame half-pel and eventually quarter-pel** — Sub-pel luma (and full chroma MC) for B is required for competitive motion compensation; today’s slice adds a bounded half-pel grid on the quarter-pel wire (`FR2` rev **14**).
2. **Weighted prediction** — Per-MB (or block) blend weights vs single average are part of H.264’s toolset; we add a small fixed candidate set and `/256` integer weights (`FR2` rev **14**) as a foundation only.
3. **MV entropy coding** — **Experimental** **`FR2` rev 17 / 18** apply **static** rANS over compact MV bytes (dual grids on **B**); this is **not** CABAC/CAVLC-class and still needs **trained / context-adaptive** models before it can be argued comparable to mature codecs’ MV coding.
4. **Transform size / mode selection** — Experimental **`FR2` rev 19+** paths add **4×4** transform units alongside legacy **8×8** chunks (signaled per partition unit); **16×16** transform remains parser-safe only unless implemented. Rich TU/CU partitioning like mature codecs is **not** present yet.
5. **Variable inter partitions** — Experimental **`FR2` rev 19/20** add bounded **P** partitions (**16×16**, **16×8**, **8×16**, **8×8**); **`AutoFast`** is a deterministic heuristic, **not** full H.264-style mode **RDO**. **`FR2` rev 21/22** (**B**) are reserved; decoder returns **`Unsupported`** until fully wired.
6. **Better intra prediction** — H.264 intra modes (9×4, DC, planar-like) and edge-aware selection are ahead of the current SRSV2 intra subset.
7. **Real RDO** — Production encoders use **full** Lagrangian RDO over modes, references, and QP. **`bench_srsv2 --rdo fast`** exercises a **bounded** heuristic (λ × estimated signaling/residual bits) over a **small** candidate set; it is **not** production-grade joint RDO.
8. **Bitrate-matched benchmarking** — CRF-only or fixed-QP sweeps are not enough: **achieved bitrate** and **objective quality** must be reported together; **target-bitrate** or **two-pass** style matching (or sweeps) is required for defensible comparison. The `bench_srsv2` tool documents x264 **preset**, **CRF**, **achieved bitrates**, **PSNR/SSIM**, and a reproducible **FFmpeg** command when FFmpeg is available; `--match-x264-bitrate` **errors immediately** until a real matching loop exists.
9. **10-bit / HDR** — 8-bit 4:2:0 SDR is not the only target for “competitive” video; extended bit depth and transfer functions are out of scope for the current core path.
10. **Tile / threaded 8K** — Massive parallel decode (tiles, slices, wavefront) and encoder threading are not implemented for production-scale 8K on CPU.
11. **GPU acceleration** — No GPU MC, transform, or entropy yet; competitive turnarounds at high resolution assume hardware-friendly pipelines.

For current bench discipline and B-frame options, see `docs/srsv2_benchmarks.md` and `docs/motion_search.md`.

**Near-term engineering backlog (after variable partitions + early TX):** **B** variable partitions (**rev 21/22**) end-to-end; **quarter-pel** luma MC beyond half-grid; **context-adaptive / trained** entropy (MV + partitions + residuals); richer **intra** prediction; bitrate-matched **x264** sweeps (**`--match-x264-bitrate`** remains **unimplemented** — fail-fast); tiled/threaded **8K** decode; GPU MC/transform/entropy (out of scope today).
