# CTU64 Superblock Plan

This is the next **HEVC-class architecture layer** for SRSV2 planning. It does **not** introduce a bitstream syntax, does **not** assign any `FR2` revisions, and does **not** change current encoder or decoder behavior.

## Scope

`crates/libsrs_video/src/srsv2/ctu64.rs` is a complete geometry scaffold:

- `CtuSize`: `Ctu16`, `Ctu32`, `Ctu64`
- `CtuGrid`: validated frame split into CTU columns / rows / count
- `CtuAddress`: raster CTU location
- `CtuBounds`: half-open luma and YUV420 chroma bounds
- `CtuError`: hostile-input validation failures
- `split_frame_into_ctu_grid`
- `map_ctu_to_yuv420_bounds`

The module is intentionally limited to safe planning primitives. It should be usable by future RDO, partition, threading, tiling, or telemetry work without implying that any CTU map is on the wire.

## Non-Goals

- No encoder syntax.
- No decoder syntax.
- No `FR2` revision.
- No partition tree encoding.
- No recursive quadtree decisions.
- No transform-size changes.
- No rate-control behavior changes.
- No claim that SRSV2 beats H.265/HEVC or x265.

## Why CTU64

Current SRSV2 work is still largely macroblock-scale: fixed 16x16, experimental variable partitions, and map v2 measurement. The HEVC-class gap list calls out larger coding-tree structure as a real architectural step before deeper syntax work.

A CTU64 layer gives future code a shared coordinate system for:

- larger prediction regions,
- bounded quadtree partition search,
- per-superblock telemetry,
- future threading/tile boundaries,
- luma/chroma-safe edge handling for non-multiple-of-64 dimensions.

## Geometry Rules

Frame dimensions are validated against existing SRSV2 hostile-input caps:

- width and height must be non-zero,
- each dimension must be within `MAX_DIMENSION`,
- luma samples must be within `MAX_LUMA_SAMPLES`,
- CTU count must remain below `MAX_CTU_COUNT`.

Grid columns and rows use ceiling division by the CTU luma edge:

```text
cols = ceil(width / ctu_edge)
rows = ceil(height / ctu_edge)
count = cols * rows
```

Each CTU uses half-open luma bounds:

```text
x0 = x_ctu * ctu_edge
y0 = y_ctu * ctu_edge
x1 = min(x0 + ctu_edge, width)
y1 = min(y0 + ctu_edge, height)
```

YUV420 chroma bounds use half-resolution coordinates with ceil-divided end bounds:

```text
chroma_x0 = floor(luma_x0 / 2)
chroma_y0 = floor(luma_y0 / 2)
chroma_x1 = ceil(luma_x1 / 2)
chroma_y1 = ceil(luma_y1 / 2)
```

This keeps partial edge CTUs safe for odd frame dimensions and prevents chroma reads/writes outside the allocated U/V planes.

## Initial Test Coverage

The Rust module tests verify:

- `128x128` splits into four `64x64` CTUs,
- `1920x1080` produces safe partial edge CTUs,
- 8K UHD grid counts remain bounded,
- zero dimensions are rejected,
- oversized / overflowing dimensions are rejected,
- YUV420 chroma bounds are valid, including odd edge dimensions.

## Future Work

Later blocks can build on this only after separate design decisions:

1. Bounded quadtree partition search inside each CTU.
2. CTU-level cost accounting and telemetry.
3. CTU-to-existing macroblock bridge for backward-compatible experiments.
4. Syntax proposal with explicit revisioning and decoder fallback rules.
5. Dedicated tests proving old `FR2` revisions still decode unchanged.

Until those are designed, `ctu64.rs` must remain a geometry scaffold only.
