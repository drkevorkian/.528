# SRS Media System Workspace

This repository is a greenfield Rust workspace that provides:

- shared media contracts (`libsrs_contract`)
- compatibility probe and ingest layer (`libsrs_compat`; optional FFmpeg behind `ffmpeg` feature)
- integration-oriented pipeline facade (`libsrs_pipeline`)
- shared application services (`libsrs_app_services`) including `.528` **playback session** (demux + decode)
- shared config and licensing protocol/client crates
- command line entrypoint (`apps/srs_cli`)
- dedicated admin desktop UI (`apps/srs_admin`)
- dual-workspace desktop player UI (`apps/srs_player`)
- same-repo licensing server and website (`apps/srs_license_server`)

The workspace stays buildable while codec and GPU features grow; see the status table below for what is real today.

## Native `.528` container and import

- **Multiplexed native files** should use the **`.528`** extension (v2 `SRS528\0\0`); **`.srsm`** is still accepted for the same bitstream (including legacy v1 headers—see `docs/528_container_format.md`).
- **Analyze / mux / demux** operate on these paths through `libsrs_demux` / `libsrs_mux`.
- **Import** (`srs_cli` import, `libsrs_app_services`) ingests packets, **decodes** native SRS video/audio to normalized frames (`MediaDecoder`), then **re-encodes** into `.528` via `NativeEncoderSink`. **Non-native** paths require **`libsrs_compat` with `ffmpeg`**; the stub backend does not fabricate video.

Further detail: `docs/specs/compatibility_layer.md`, `docs/specs/container_format.md` (index), and `docs/528_container_format.md`.

## Implementation status

| Area | Status | Notes |
|------|--------|--------|
| `.528` container | **Partial / working** | v2 primary; hostile-input limits in I/O (`libsrs_container`) |
| mux / demux | **Partial / working** | `libsrs_mux` / `libsrs_demux`; cues + index |
| audio codec | **Working prototype** | v2 LPC stream decode in `libsrs_audio` |
| video codec | **Prototype** | Intra SRS video path in `libsrs_video`; no full inter codec matrix |
| import / transcode | **Native pipeline partial** | Raw packet/decoded path in app services; FFmpeg path feature-gated |
| playback | **Decode-preview** | `PlaybackSession` demuxes + decodes; `srs_player` shows grayscale texture only; `srs_cli play` smoke decode |
| GPU | **Planned** | No device presentation or GPU decode here |
| lossy video v2 | **Planned** | |
| admin / licensing | **Partial / working** | Needs production hardening |

Further playback architecture: `docs/playback_pipeline.md`.

## Build

```bash
cargo check
```

## Config

Default local configuration lives in `config/srs.toml`.

- client primary licensing URL: `http://localhost:3000`
- client backup licensing URL: `http://127.0.0.1:3000`
- local licensing database path: `var/srs_license.sqlite3`

`localhost` is only correct when the client and licensing server are on the same machine.
For Windows, macOS, Ubuntu, Red Hat, SUSE, or other Linux clients connecting to your
Gentoo-hosted licensing server, change:

- client `primary_url`
- client `backup_url`
- server `base_url`
- server `bind_addr` (for example `0.0.0.0:3000`)

to values reachable from the network instead of `localhost`.

## Run The Licensing Server

```bash
cargo run -p srs_license_server
```

Visit [http://localhost:3000](http://localhost:3000) to issue a basic key and confirm pending installations.

## Run The Desktop App

```bash
cargo run -p srs_player
```

The desktop app automatically falls back to play-only mode when verification is unavailable or pending.

## Launch Helpers

### Linux And macOS

Use the generic Unix launcher on:

- Gentoo
- Ubuntu
- Red Hat / RHEL-compatible systems
- SUSE-compatible systems
- macOS

```bash
bash tools/run_unix.sh
```

Useful modes:

```bash
bash tools/run_unix.sh server
bash tools/run_unix.sh --admin-ui
bash tools/run_unix.sh cli analyze path/to/file.528
bash tools/run_unix.sh cli --no-server -- analyze path/to/file.528
```

### Gentoo Compatibility Wrapper

The original Gentoo-specific wrapper still works:

```bash
bash tools/run_gentoo.sh
```

On Gentoo, the compatibility wrapper now starts the dedicated `srs_admin` desktop UI
after the licensing server starts, then launches the player. The admin UI provides:

- database stats
- license feature editing
- key activation/deactivation
- pending request approval
- installation and verification status views
- recent audit / connection log visibility

The Gentoo wrapper also defaults GUI apps to the X11 backend to avoid known Wayland
`winit` pointer crashes on some desktop setups. Override if needed:

```bash
SRS_GUI_BACKEND=wayland bash tools/run_gentoo.sh
```

### Windows

Use either PowerShell directly:

```powershell
powershell -ExecutionPolicy Bypass -File tools\run_windows.ps1
```

or the batch wrapper:

```bat
tools\run_windows.cmd
```

Windows launcher modes match the Unix launcher: `player`, `server`, and `cli`.

## Optional FFmpeg Compatibility

FFmpeg integration is isolated in `libsrs_compat` behind the `ffmpeg` feature:

```bash
cargo check -p libsrs_compat --features ffmpeg
```
