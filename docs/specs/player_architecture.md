# Player Architecture Spec

`srs_player` remains a single desktop app built with `eframe/egui`.

## UI Model

The desktop shell exposes two workspaces inside one binary:

- `PlayOnly`: safe default for playback-oriented features.
- `Editor`: unlocked by a verified editor entitlement.

Both workspaces share one top-level app state and one window chrome so the
application can move between modes without restarting.

## UX Baseline

### Play-Only Workspace

- open/close media input
- play/pause/stop controls
- skip forward/backward
- timeline seek
- current track list
- recent files
- metadata and status surfaces
- entitlement / connectivity banner

### Editor Workspace

- all play-only features
- encode/decode actions
- mux/demux/import/transcode actions
- compression/export actions
- selection and timeline controls
- frame inspection/edit scaffolding

## Layering

- UI state machine stays in the app crate.
- Playback and editor actions call into shared application services first.
- Shared application services call `libsrs_pipeline` and the native codec/container crates.
- Licensing verification is handled by a dedicated licensing client, not by low-level media crates.
- `libsrs_pipeline` remains media-oriented and does not own entitlement policy.
- Future native decode/render paths can replace placeholders without redesigning the workspace split.

## Entitlement Rules

- `PlayOnly` is always available for safe playback-oriented workflows.
- `Editor` actions are enabled only when the current signed entitlement grants editor capabilities.
- If verification fails, is offline, or returns a pending-confirmation state, the app must fall back to `PlayOnly`.
- The fallback must explain why editor actions are unavailable without blocking basic playback.
