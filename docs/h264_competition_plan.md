# SRSV2 vs mature codecs (engineering roadmap — **not** a superiority claim)

SRSV2 is an experimental native codec. Maturing codecs such as **H.264** combine decades of standardized tools. This note lists **real blockers** before SRSV2 could be argued seriously on efficiency at matched complexity constraints — **without** claiming SRSV2 “beats” H.264 today.

1. **B-frame half-pel and eventually quarter-pel** — Sub-pel luma (and full chroma MC) for B is required for competitive motion compensation; today’s slice adds a bounded half-pel grid on the quarter-pel wire (`FR2` rev **14**).
2. **Weighted prediction** — Per-MB (or block) blend weights vs single average are part of H.264’s toolset; we add a small fixed candidate set and `/256` integer weights (`FR2` rev **14**) as a foundation only.
3. **MV entropy coding** — Inter MVs are not entropy-coded like CABAC/CAVLC MV contexts; raw tuples inflate bitrate at competitive QP/rate points.
4. **Transform size / mode selection** — Fixed 8×8-ish residual paths without rich TU/CU partitioning or larger transforms limit adaptation versus H.264’s 4×4/8×8 mix (and beyond).
5. **Better intra prediction** — H.264 intra modes (9×4, DC, planar-like) and edge-aware selection are ahead of the current SRSV2 intra subset.
6. **Real RDO** — Production encoders use Lagrangian RDO over modes, references, and QP; bench paths use SAD heuristics and fixed QP, not joint rate–distortion optimization.
7. **Bitrate-matched benchmarking** — CRF-only or fixed-QP sweeps are not enough: **achieved bitrate** and **objective quality** must be reported together; **target-bitrate** or **two-pass** style matching (or sweeps) is required for defensible comparison. The `bench_srsv2` tool documents x264 **preset**, **CRF**, **achieved bitrates**, **PSNR/SSIM**, and a reproducible **FFmpeg** command when FFmpeg is available; `--match-x264-bitrate` **errors immediately** until a real matching loop exists.
8. **10-bit / HDR** — 8-bit 4:2:0 SDR is not the only target for “competitive” video; extended bit depth and transfer functions are out of scope for the current core path.
9. **Tile / threaded 8K** — Massive parallel decode (tiles, slices, wavefront) and encoder threading are not implemented for production-scale 8K on CPU.
10. **GPU acceleration** — No GPU MC, transform, or entropy yet; competitive turnarounds at high resolution assume hardware-friendly pipelines.

For current bench discipline and B-frame options, see `docs/srsv2_benchmarks.md` and `docs/motion_search.md`.
