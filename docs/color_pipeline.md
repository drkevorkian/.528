# Color pipeline (SRSV2)

## BT.709 (primary)

CPU conversion for RGB888 → YUV420p8 uses BT.709 coefficients with selectable **limited** vs **full** range (`ColorRange`). Chroma is derived via 2×2 averaging on subsampled planes.

Preview path YUV420 → RGB888 uses limited-range assumptions suitable for UI preview.

## BT.2020 readiness

Enums (`ColorPrimaries`, `MatrixCoefficients`) include BT.2020 values; matrix math for production BT.2020 paths remains future work once 10-bit buffers are wired end-to-end.

## CLI raw layouts

`srs_cli encode --codec srsv2` accepts tightly packed **rgb8**, **rgba8**, or **bgra8** frames row-major; alpha is ignored after RGB extraction.

Decoding `.srsv2` via `libsrs_app_services::decode_native_to_raw` writes **planar YUV420**: full Y, then U, then V (tight rows), concatenated across frames in demux order.
