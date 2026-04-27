# ADR-0002: Native vs Compatibility Layer

## Status
Accepted

## Decision
Keep external compatibility ingestion (including FFmpeg) in `libsrs_compat`, and keep native pipeline APIs in `libsrs_pipeline`.

## Consequences
- Reduced blast radius from third-party dependencies.
- Ability to disable compatibility backends in hardened builds.
