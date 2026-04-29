# Native multiplexed container

The **normative** on-disk layout for current writers and readers is documented in **[`.528` container format (v2)](../528_container_format.md)** (`docs/528_container_format.md`).

## Filename extension

- **Primary:** **`.528`** (v2 magic `SRS528\0\0`).
- **Legacy:** **`.srsm`** — same bitstream family; readers accept v1 (`SRSM` magic) and v2.

CLI, samples, and tests treat **`.528`** as the default when creating new files.

## Related specs

- [Audio bitstream](audio_bitstream.md) — elementary `.srsa` and frame payloads inside container packets.
- [Video bitstream](video_bitstream.md) — native video elementary layout.
- [Compatibility layer](compatibility_layer.md) — probe/ingest and import into native containers.

## Historical note

An earlier draft of this file described **v1-only** header layout. That content is **obsolete**; use `528_container_format.md` for v1 vs v2 header sizes, magic values, and track/block tables.
