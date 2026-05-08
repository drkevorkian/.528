# Partition syntax v2 (SRSV2 — standalone map + MV-share blobs)

This document defines **experimental** on-disk structures for:

1. **Partition map v2** — compact encoding of per-macroblock inter partition modes.
2. **MV share groups v2** — optional grouping of partition-unit (PU) indices that share one MV delta payload in a higher layer (not specified here).

Blobs are embedded in **FR2** rev **27** / **28** when the encoder selects **`SrsV2PartitionSyntaxMode::V2RleMvShare`** (see `docs/video_bitstream_v2.md`). The default **`V1Legacy`** mode keeps prior **FR2** rev **19**/**20**/**25** map layouts (`SrsV2PartitionMapEncoding`). **Decoders for rev 19/20 (and other pre–map-v2 revisions) are unaffected** — they never parse **`S2P1`** / **`S2G1`** blobs; **v2** is an additional wire path only when rev **27**/**28** is present.

## Rust API (`crates/libsrs_video/.../partition_syntax_v2.rs`)

Complete public surface:

- **`encode_partition_map_v2`** / **`decode_partition_map_v2`**
- **`encode_mv_share_groups_v2`** / **`decode_mv_share_groups_v2`**
- **`estimate_partition_syntax_v2_bytes`**
- **`validate_partition_map_v2`**

Supporting helpers: **`v1_legacy_partition_map_bytes`**, **`total_pu_slots_for_modes`**, types **`PartitionMapV2`**, **`PartitionModeV2`**, **`MvShareGroupV2`**, **`PartitionSyntaxV2Stats`**, **`PartitionSyntaxV2Error`**.

## Engineering benchmark (`bench_srsv2 --compare-partition-syntax`)

Requires **`--inter-syntax compact`**, **`--bframes 0`** (validator). Runs five passes in fixed order; each JSON row includes:

`partition_syntax_mode`, `partition_map_v1_bytes`, `partition_map_v2_bytes`, `mv_share_group_count`, `mv_share_bytes`, `partition_syntax_savings_bytes`, `partition_syntax_savings_percent`, `total_bytes`, `psnr_y`, `ssim_y` (plus `encode_fps` / `decode_fps` / `row` id).

| Row id | Role |
| --- | --- |
| **`fixed16x16`** | Fixed partition; report row uses **`v1`** map only (`partition_map_v2_bytes` = **0** — no v2 map on wire for this mode). |
| **`auto-fast-rdo-v1`** | **`inter-partition auto-fast`**, **`partition-cost-model rdo-fast`**, **`rdo fast`**, **`partition-syntax v1`**. |
| **`auto-fast-rdo-v2`** | Same as above with **`partition-syntax v2`**. |
| **`split8x8-v1`** | **`inter-partition split8x8`**, **`partition-syntax v1`**. |
| **`split8x8-v2`** | **`inter-partition split8x8`**, **`partition-syntax v2`**. |

For **AutoFast** and **split8×8** pairs, **`partition_map_v1_bytes`** and **`partition_map_v2_bytes`** are filled from both passes so **`partition_syntax_savings_*`** reflect **map** byte deltas. **Sparse** split maps (mostly **16×16**, rare splits) are intended to show **v2** smaller than **v1**; **uniform 16×16** at large **`n_mb`** uses the **UNIFORM** kind (**6 bytes**) vs **`n_mb`** legacy bytes.

## Unit-test coverage (summary)

The module tests include, among others:

- **All-16×16** on a typical grid: **v2** wire **strictly smaller** than v1; **6-byte** uniform header on an 8×8 MB grid.
- **Mostly 16×16** with a rare **8×8** split: **v2** smaller than v1 (**RLE**).
- **All-8×8** and **mixed** maps: **roundtrip**.
- **Invalid mode** wire byte, **zero RLE run length**, **zero `n_runs`**, **RLE length mismatch** (short / overflow), **truncated** map, **trailing** bytes after map.
- **MV-share**: **roundtrip**; **missing PU leaf**; **duplicate PU inside group** (`MvShareGroupV2::new` and decode); **duplicate across groups**; malicious decode blob with duplicate members.

## `FR2` embedding (rev **27** / **28**)

After `frame_index`, `qp`, **`flags`**, and optional **`clip_min` / `clip_max`**:

- **Rev 27** (compact MV): **`u32` LE** length of one **`S2P1`** map blob, map bytes, **`u32` LE** length of **`S2G1`** MV-share blob (often **0**), optional share bytes, then partitioned compact MV bytes (same as rev **19**).
- **Rev 28** (entropy MV): **`u8`** selector **0** = StaticV1 rANS MV blob, **1** = ContextV1; then **`sym_count` / `blob_len` / blob** as rev **20**/**25**. Map and MV-share prefix match rev **27**.

**`P_INTER_FLAG_PACKED_PART_MAP` (bit 3)** must be **clear** for rev **27**/**28**.

## Goals

- **Uniform 16×16 frames**: a handful of bytes for the whole map (not `O(n_mb)` legacy bytes).
- **Mostly 16×16 with rare 8×8**: run-length encoding (RLE) avoids per-leaf overhead when splits cluster.
- **MV sharing** (optional): declare equivalence classes of PU indices so one coded MV delta can legally predict others (encoder policy; this layer only validates indices).

## Macroblock modes (`PartitionModeV2`)

Logical modes match existing **P** wire constants (`inter_mv.rs`, low two bits only):

| Wire `u8` | Mode |
| ---: | --- |
| 0 | 16×16 |
| 1 | 16×8 |
| 2 | 8×16 |
| 3 | 8×8 |

Upper bits **must** be zero on the wire inside map payloads.

## PU index space (MV sharing)

For a frame with `mb_cols × mb_rows` macroblocks:

1. Walk macroblocks in **raster order** (width major: `mbx` then `mby`).
Per-macroblock PU order matches `p_var_partition::candidate_sad_and_mvs` / `inter_mv::encode_mv_stream_partitioned`:

- **16×16** — one PU.
- **16×8** — top half (`dy=0`), then bottom (`dy=8`).
- **8×16** — left (`dx=0`), then right (`dx=8`).
- **8×8** — `(0,0), (8,0), (0,8), (8,8)` (column-major pairs in luma).
3. Global PU index = sequential index from 0 .. `total_pu_count - 1`.

`total_pu_count` is the sum over MBs of `pu_count(mode(mb))` (1, 2, 2, or 4).

MV-share groups reference **global PU indices** only.

## Partition map v2 wire format

All multi-byte integers are **little-endian**.

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 4 | Magic **`S2P1`**: `0x53 0x32 0x50 0x01` |
| 4 | 1 | **Kind** |
| … | … | **Body** (depends on kind) |

### Kind `0` — **UNIFORM**

Every macroblock uses the same mode. `n_mb = mb_cols * mb_rows` is implied by the decoder call; it is **not** repeated on the wire.

| Offset | Size | Field |
| --- | --- | --- |
| 5 | 1 | Mode byte (`0..=3`, no reserved bits) |

**Total size:** 6 bytes for any uniform map.

### Kind `1` — **RLE**

| Offset | Size | Field |
| --- | --- | --- |
| 5 | 2 | `n_runs` (must satisfy `1 <= n_runs <= n_mb`) |
| 7 | 3 × `n_runs` | For each run: `u8 mode`, `u16 count` |

Each `count` must be **non-zero**. The sum of all `count` must equal **`n_mb`**. Runs describe MBs in raster order.

### Kind `2` — **RAW_LEGACY_EMBED**

Micro-path for very small maps where a 6-byte uniform header would exceed **legacy** `n_mb` bytes.

| Offset | Size | Field |
| --- | --- | --- |
| 5 | `n_mb` | Raw mode bytes (same as v1 legacy one-byte-per-MB) |

**Constraint:** `n_mb <= 5` only. Decoder rejects kind `2` when `n_mb > 5`.

### Trailing data

After a successful parse, the cursor must equal `data.len()`. **Trailing bytes are an error**.

## MV share groups v2 wire format

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 4 | Magic **`S2G1`**: `0x53 0x32 0x47 0x01` |
| 4 | 2 | `n_groups` |
| 6 | … | Repeated `n_groups` times: see below |

Each group:

| Field | Type | Notes |
| --- | --- | --- |
| `n_members` | `u16` | `>= 2` |
| `members` | `n_members × u16` | Global PU indices ( **`0..65535`** wire range); **first** member is the **representative** |

Rules:

- Every index must be `< total_pu_slots` passed into decode.
- No index may appear in more than one group.
- **Duplicate inside one group** = error.

## Reference: v1 legacy map size (comparison)

Legacy **one-byte-per-macroblock** map size = **`n_mb`** bytes.

## Security bounded decode

Implementations reject overflow, zero runs, bad magic, and trailing garbage without panicking on hostile input.
