# ADR-0002: Native vs Compatibility Layer

## Status
Accepted

## Decision
Keep external compatibility ingestion (including FFmpeg) in `libsrs_compat`, and keep native pipeline APIs in `libsrs_pipeline`.

## Consequences
- Reduced blast radius from third-party dependencies.
- Ability to disable compatibility backends in hardened builds.
- Probe results carry **optional audio layout** (`sample_rate`, `channels`) for native import; import encodes through **`libsrs_mux` / native codecs** rather than ad-hoc placeholders when wired through `libsrs_app_services`.
