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
- **Import** (`srs_cli import --codec srsv2|srsv1`, `libsrs_app_services`) ingests packets, **decodes** native SRS video/audio to normalized frames (`MediaDecoder`), then **re-encodes** into `.528` via `NativeEncoderSink`. **Default video** for new containers is **SRSV2** (`codec_id` 3); use **`--codec srsv1`** for legacy SRSV1. **Non-native** paths require **`libsrs_compat` with `ffmpeg`**; the stub backend does not fabricate video.

Further detail: `docs/specs/compatibility_layer.md`, `docs/specs/container_format.md` (index), and `docs/528_container_format.md`. SRSV2 roadmap: `docs/srsv2_design_targets.md`. Optional reproducible measurement notes: `docs/srsv2_benchmarks.md`.

## Implementation status

| Area | Status | Notes |
|------|--------|--------|
| `.528` container | **Partial / working** | v2 primary; hostile-input limits in I/O (`libsrs_container`) |
| mux / demux | **Partial / working** | `libsrs_mux` / `libsrs_demux`; cues + index; mux prefers `.srsv2` when elementary video is present |
| audio codec | **Working prototype** | v2 LPC stream decode in `libsrs_audio` |
| video codec | **SRSV2 default** | Modern native **8K-first** direction (`docs/srsv2_design_targets.md`). Today: CLI square-gray → `.srsv2` **single intra** (`FR2\x01`). **Native import** (SRSV2) uses **`max_ref_frames = 1`** and **P** (`FR2\x02`) after first picture when dimensions are **16-aligned**. Profiles **Baseline…Research** on-wire; most helpers still emit **Main**. Rate control, sub-pel/B/GPU/OS A/V remain roadmap. |
| import / transcode | **Native pipeline partial** | Encode/import/transcode default to SRSV2 video; `--codec srsv1` selects legacy; FFmpeg path feature-gated |
| playback | **Decode-preview** | `PlaybackSession` decodes SRSV2 **intra** + experimental **P** (`codec_id` **3**, `FR2\x02` when a reference slot is filled), and SRSV1 (`codec_id` **1**); **SRSA audio** is `codec_id` **2**. OS audio/video presentation is **not** implemented; `srs_player` shows last-frame texture; `srs_cli play` smoke-decodes |
| GPU | **Planned** | No device presentation or GPU decode here |
| lossy video v2 | **Planned** | |
| admin / licensing | **Partial / working** | Needs production hardening |

Further playback architecture: `docs/playback_pipeline.md`.

### Benchmark tooling (optional, engineering measurements)

- **Synthetic YUV:** `cargo run -p quality_metrics --bin gen_synthetic_yuv -- --pattern noise --seed 1 --out-yuv clip.yuv --out-meta clip.json`
- **SRSV2 vs optional libx264:** `cargo run -p codec_compare -- --yuv clip.yuv --meta clip.json --out-json report.json --out-md report.md`  
  If `ffmpeg` is not on `PATH`, the harness still reports SRSV2 encode/decode metrics and skips H.264.
- These outputs are **lab measurements** — do **not** treat them as proof SRSV2 “beats” another codec without your own methodology (`docs/srsv2_benchmarks.md`).

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
