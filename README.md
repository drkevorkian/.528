# SRS Media System Workspace

This repository is a greenfield Rust workspace that provides:

- shared media contracts (`libsrs_contract`)
- compatibility probe and ingest layer (`libsrs_compat`; optional FFmpeg behind `ffmpeg` feature)
- integration-oriented pipeline facade (`libsrs_pipeline`)
- shared application services (`libsrs_app_services`)
- shared config and licensing protocol/client crates
- command line entrypoint (`apps/srs_cli`)
- dedicated admin desktop UI (`apps/srs_admin`)
- dual-workspace desktop player UI (`apps/srs_player`)
- same-repo licensing server and website (`apps/srs_license_server`)

The workspace is intentionally scaffolded to remain buildable while codec/container internals are completed by parallel agents.

## Native `.528` container and import

- **Multiplexed native files** should use the **`.528`** extension (v2 `SRS528\0\0`); **`.srsm`** is still accepted for the same bitstream (including legacy v1 headers—see `docs/528_container_format.md`).
- **Analyze / mux / demux** operate on these paths through `libsrs_demux` / `libsrs_mux`.
- **Import** (`srs_cli` import, `libsrs_app_services`) pulls packets from **`MediaIngestor`**, uses **`MediaProbe`** metadata (including **audio sample rate and channel count** when known), and writes a native container using **real** `libsrs_video` / `libsrs_audio` frame encoders—not a fake fixed-grid transcoder.

Further detail: `docs/specs/compatibility_layer.md`, `docs/specs/container_format.md` (index), and `docs/528_container_format.md`.

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
bash tools/run_unix.sh cli analyze sample.foreign
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
