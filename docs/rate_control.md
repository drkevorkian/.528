# Rate control (SRSV2)

## Current state

`SrsV2EncodeSettings` in `libsrs_video` captures intended knobs (quality, QP hints, target bitrate, keyframe interval, tune presets). The intra baseline encoder uses a **scalar QP** per frame chosen by CLI `--quality` (mapped into QP with clamping).

## Planned

Closed-loop bitrate adaptation: rolling average of compressed frame sizes vs target, optional spatial QP deltas from activity maps, separate profiles for low-latency vs archival.
