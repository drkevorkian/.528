# Architecture Overview

The workspace is split into three media integration tiers:

1. `libsrs_contract`: stable shared data contracts and timebase types.
2. `libsrs_compat`: compatibility abstraction for probing and ingesting media sources, with optional FFmpeg.
3. `libsrs_pipeline`: pipeline facade that bridges source ingest to native codec/container operations.

Applications (`srs_cli`, `srs_player`) consume `libsrs_pipeline` and avoid direct dependency on backend-specific ingest details.

Application-level policy layers may sit above the media tiers:

- shared app services for desktop/CLI orchestration
- configuration loading
- licensing verification and entitlement handling
- same-repo server and website components for key issuance and validation

## Security and Isolation

- External decoder/container stacks are isolated behind traits.
- Optional FFmpeg usage is gated by cargo feature.
- **`unsafe` is limited** to the optional FFmpeg backend (reading codec parameters for probe metadata). Default builds do not use it.
- Runtime entitlements are enforced above the codec/container crates so media formats remain free of licensing business logic.

## Native container and import

- Multiplexed output and round-trips use the **`.528`** extension by default; **`.srsm`** remains a supported legacy name for the same format family (see [`.528` container format](../528_container_format.md)).
- **Import** normalizes foreign or stub sources through `libsrs_compat` ingest, then muxes with **`libsrs_video`** / **`libsrs_audio`** encode APIs via **`libsrs_app_services`** orchestration.
