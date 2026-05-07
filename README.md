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

Further detail: `docs/specs/compatibility_layer.md`, `docs/specs/container_format.md` (index), and `docs/528_container_format.md`. SRSV2 roadmap: `docs/srsv2_design_targets.md`. **HEVC-class** engineering gap list (honest, no superiority claims): `docs/hevc_competition_plan.md`. Legacy **H.264-class** gap note: `docs/h264_competition_plan.md`. Optional reproducible measurement notes: `docs/srsv2_benchmarks.md`.

## Implementation status

| Area | Status | Notes |
|------|--------|--------|
| `.528` container | **Partial / working** | v2 primary; hostile-input limits in I/O (`libsrs_container`) |
| mux / demux | **Partial / working** | `libsrs_mux` / `libsrs_demux`; cues + index; mux prefers `.srsv2` when elementary video is present |
| audio codec | **Working prototype** | v2 LPC stream decode in `libsrs_audio` |
| video codec | **SRSV2 default** | Modern native **8K-first** direction (`docs/srsv2_design_targets.md`). Today: CLI square-gray → `.srsv2` **single intra** (`FR2\x01`). Encoders may emit **intra with adaptive residual entropy** (`FR2\x03`, experimental) and optional **block-level QP deltas** (`FR2\x07`–`\x09`, experimental; see `docs/video_bitstream_v2.md`). **Native import** (SRSV2) uses **`max_ref_frames = 1`** and **P** (`FR2\x02` / **`FR2\x04`** integer MV; optional experimental **`FR2\x05` / `FR2\x06`** half-pel; **`FR2\x08` / `FR2\x09`** with block AQ — see `docs/motion_search.md`). **Experimental B** (`FR2` rev **10**/**11**/**13** — **`bench_srsv2 --bframes 1`** emits **rev 13** per-MB blend) and **alt-ref** (rev **12**) are **parser-safe / baseline** in `libsrs_video` and playback — **not** parity with mature codecs and **not** a superiority claim vs **H.264/AVC** or **H.265/HEVC-class** encoders (see `docs/hevc_competition_plan.md`). **Inter MV entropy:** **StaticV1** is default (**rev 17**/**18**/**20** when compact/entropy paths are selected); **ContextV1** (**rev 23**/**24**/**25**) is **experimental** Rust-API opt-in — see **`docs/video_bitstream_v2.md`**. **Rev 26** is **unsupported** decode today. Profiles **Baseline…Research** on-wire; most helpers still emit **Main**. **First-pass deterministic rate control** exists for benchmark / encoder-side QP selection (`SrsV2RateController`, `bench_srsv2`; not production-tuned). **Full quarter-pel, production B/GOP tuning, GPU encode/decode, and OS audio/video output** remain roadmap. |
| import / transcode | **Native pipeline partial** | Encode/import/transcode default to SRSV2 video; `--codec srsv1` selects legacy; FFmpeg path feature-gated |
| playback | **Decode-preview** | `PlaybackSession` uses a bounded **`SrsV2ReferenceManager`** for `codec_id` **3**: **intra** (`FR2` rev **1**/**3**/**7**), experimental **P** (rev **2**/**4**/**5**/**6**/**8**/**9**), experimental **B** (`FR2` rev **10**/**11**/**13**) when **`max_ref_frames ≥ 2`** and the stream’s **packet order** supplies both anchors before the **B** (typical **decode order** *I₀ → P₂ → B₁* — not presentation order), experimental **alt-ref** (rev **12**, non-displayable). **B** with **`max_ref_frames < 2`** returns a clear **unsupported** error. SRSV1 (`codec_id` **1**) stays grayscale intra; **SRSA audio** is `codec_id` **2**. OS A/V output is **not** implemented; `srs_player` shows last-frame texture; `srs_cli play` smoke-decodes |
| GPU | **Planned** | No device presentation or GPU decode here |
| lossy video v2 | **Planned** | |
| admin / licensing | **Partial / working** | Needs production hardening |

Further playback architecture: `docs/playback_pipeline.md`.

**SRSV2 experimental status (short):** **B-frame** syntax — experimental (rev **10**/integer MV + frame blend, rev **11**/half-pel, rev **13**/per-MB blend + integer MV, rev **14**/per-MB blend + half-pel MV grid + optional weighted candidates). **Compact / StaticV1 / ContextV1 inter MV** — **`FR2` rev 15**/**16** (compact), **17**/**18**/**20** (**StaticV1** rANS MVs), **23**/**24**/**25** (**ContextV1** rANS MVs; **experimental**, **not** CABAC-class, **not** default — `SrsV2EncodeSettings::entropy_model_mode`); full revision table: **`docs/video_bitstream_v2.md`**. **`bench_srsv2 --inter-syntax`** exercises **raw / compact / entropy** with default **`StaticV1`** on the entropy path. **Variable P-frame inter partitions** — experimental **`FR2` rev 19/20** (+ **25** with **ContextV1**); **B** rev **21**/**22** and **rev 26** decode as **`Unsupported`** (placeholders / reserved). Default encoding stays **fixed 16×16**; **`bench_srsv2 --inter-partition`**, **`--transform-size`**, **`--compare-partitions`** (see `docs/srsv2_benchmarks.md`). **`bench_srsv2 --bframes 1`** uses rev **13** or **14** depending on **`--b-motion-search`** / **`--b-weighted-prediction`** (unless **`--inter-syntax`** selects **16**/**18**). **`--compare-inter-syntax`**, **`--compare-rdo`**, and **`--compare-partitions`** are mutually exclusive batch modes (see `docs/srsv2_benchmarks.md`). **`--compare-b-modes`** runs **P-only**, **B-int**, **B-half**, and **B-weighted** rows in one report (see `docs/srsv2_benchmarks.md`). **Alt-ref** (rev **12**) — experimental non-display reference; **`bench_srsv2 --alt-ref on`** is rejected (“not wired yet”) so reports stay honest. **Benchmark B-GOP** (`bench_srsv2 --bframes 1`, keyint-aware **I/B/P** placement, decode order may be *I₀→P₂→B₁…*, metrics in display/`frame_index` order; **`--bframes > 1`** unsupported). Optional **`--b-motion-search independent-forward-backward`** enables integer B ME (rev **13**); **`independent-forward-backward-half`** enables half-pel B refinement (rev **14**). Optional **`--b-weighted-prediction`** enables weighted blend candidates (rev **14** wire). **B** RDO and production GOP placement remain future work. **No claim** that SRSV2 “beats” **H.264**, **H.265/HEVC**, or any mature encoder — see **`docs/hevc_competition_plan.md`** for the **HEVC-class** gap list; **`docs/h264_competition_plan.md`** remains an auxiliary AVC-oriented note.

### Benchmark tooling (optional, engineering measurements)

- **Synthetic YUV** (`--out` / `--meta`):

  ```bash
  cargo run -p quality_metrics --bin gen_synthetic_yuv -- \
    --pattern flat --width 128 --height 128 --frames 30 --fps 30 --seed 1 \
    --out var/bench/flat.yuv --meta var/bench/flat.json
  ```

- **SRSV2 core benchmark** (primary path; no FFmpeg required):

  ```bash
  cargo run -p quality_metrics --bin bench_srsv2 -- \
    --input var/bench/flat.yuv --width 128 --height 128 --frames 30 --fps 30 \
    --qp 28 --keyint 30 --motion-radius 16 \
    --residual-entropy auto \
    --subpel off --subpel-refinement-radius 1 \
    --report-json var/bench/flat_srsv2.json --report-md var/bench/flat_srsv2.md
  ```

- **Experimental B-GOP benchmark** (`--bframes 1` only in this slice; requires **`--reference-frames ≥ 2`**, **`--frames ≥ 3`**, 16-aligned size; mutually exclusive with **`--sweep`** / **`--compare-residual-modes`**). Optional **`--b-motion-search independent-forward-backward`** enables integer **B** ME (**FR2** rev **13**); default **`off`** keeps zero MVs for **B** while still choosing per-MB blend by SAD:

  ```bash
  cargo run -p quality_metrics --bin gen_synthetic_yuv -- \
    --pattern moving-square --width 128 --height 128 --frames 30 --fps 30 --seed 2 \
    --out var/bench/mov.yuv --meta var/bench/mov.json
  cargo run -p quality_metrics --bin bench_srsv2 -- \
    --input var/bench/mov.yuv --width 128 --height 128 --frames 30 --fps 30 \
    --qp 28 --keyint 30 --motion-radius 16 --residual-entropy auto \
    --reference-frames 2 --bframes 1 --b-motion-search off \
    --report-json var/bench/mov_b.json --report-md var/bench/mov_b.md
  ```

- **Optional libx264 comparison** (requires `ffmpeg` on `PATH`; useful sanity baseline — **not** a substitute for **HEVC-class** targets; see `docs/hevc_competition_plan.md`):

  ```bash
  cargo run -p quality_metrics --bin bench_srsv2 -- \
    --input var/bench/flat.yuv --width 128 --height 128 --frames 30 --fps 30 \
    --qp 28 --keyint 30 --motion-radius 16 --compare-x264 --x264-crf 23 --x264-preset medium \
    --report-json var/bench/flat_srsv2.json --report-md var/bench/flat_srsv2.md
  ```

- **Residual entropy A/B/C** (`explicit` vs `auto` vs forced `rans`) in one report (no FFmpeg):

  ```bash
  cargo run -p quality_metrics --bin bench_srsv2 -- \
    --input var/bench/flat.yuv --width 128 --height 128 --frames 30 --fps 30 \
    --qp 28 --keyint 30 --motion-radius 16 --compare-residual-modes \
    --report-json var/bench/flat_residual_cmp.json --report-md var/bench/flat_residual_cmp.md
  ```

- **Inter-syntax & RDO measurement rows** (experimental; **raw** remains default for single-pass benches):

  ```bash
  cargo run -p quality_metrics --bin bench_srsv2 -- \
    --input var/bench/flat.yuv --width 128 --height 128 --frames 30 --fps 30 \
    --qp 28 --keyint 30 --motion-radius 16 --residual-entropy auto \
    --compare-inter-syntax \
    --report-json var/bench/flat_inter_syn.json --report-md var/bench/flat_inter_syn.md
  cargo run -p quality_metrics --bin bench_srsv2 -- \
    --input var/bench/flat.yuv --width 128 --height 128 --frames 30 --fps 30 \
    --qp 28 --keyint 30 --motion-radius 16 --residual-entropy auto \
    --compare-rdo \
    --report-json var/bench/flat_rdo_cmp.json --report-md var/bench/flat_rdo_cmp.md
  ```

- **Variable P-frame partitions (experimental rev **19**/**20** wire):** **`--inter-syntax compact`** (or **`entropy`**) required when **`--inter-partition`** is not **`fixed16x16`**. **`--compare-partitions`** batches **fixed16x16**, **split8x8**, **auto-fast**:

  ```bash
  cargo run -p quality_metrics --bin bench_srsv2 -- \
    --input var/bench/flat.yuv --width 128 --height 128 --frames 30 --fps 30 \
    --qp 28 --keyint 30 --motion-radius 16 --residual-entropy explicit \
    --inter-syntax compact --compare-partitions \
    --report-json var/bench/flat_part_cmp.json --report-md var/bench/flat_part_cmp.md
  ```

- **Rate control knobs** (encoder-side QP selection for the benchmark loop; see `docs/rate_control.md`):

  ```bash
  # Constant-quality style: `--quality` is used as the QP index (after min/max clamp).
  cargo run -p quality_metrics --bin bench_srsv2 -- \
    --input var/bench/flat.yuv --width 128 --height 128 --frames 30 --fps 30 \
    --rc quality --quality 22 --keyint 30 --motion-radius 16 --residual-entropy auto \
    --report-json var/bench/flat_cq.json --report-md var/bench/flat_cq.md

  # Target bitrate (first-pass per-frame adaptation; achieved bitrate is reported vs target).
  cargo run -p quality_metrics --bin bench_srsv2 -- \
    --input var/bench/flat.yuv --width 128 --height 128 --frames 30 --fps 30 \
    --rc target-bitrate --target-bitrate-kbps 800 --qp 28 --min-qp 10 --max-qp 40 --qp-step-limit 2 \
    --keyint 30 --motion-radius 16 --residual-entropy auto \
    --report-json var/bench/flat_tb.json --report-md var/bench/flat_tb.md
  ```

- **Adaptive quantization & motion (experimental)** — frame-level AQ and integer-pel motion modes (`docs/adaptive_quantization.md`, `docs/motion_search.md`); reports include AQ stats and motion aggregates:

  ```bash
  cargo run -p quality_metrics --bin bench_srsv2 -- \
    --input var/bench/flat.yuv --width 128 --height 128 --frames 30 --fps 30 \
    --qp 28 --keyint 30 --motion-radius 16 --residual-entropy auto \
    --aq activity --aq-strength 4 --motion-search diamond --early-exit-sad-threshold 0 \
    --enable-skip-blocks \
    --report-json var/bench/flat_aq.json --report-md var/bench/flat_aq.md
  ```

- **Benchmark sweep** (fixed QP grid over QP × residual × motion radius; JSON array + Markdown table):

  ```bash
  cargo run -p quality_metrics --bin bench_srsv2 -- \
    --input var/bench/flat.yuv --width 128 --height 128 --frames 30 --fps 30 \
    --keyint 30 --sweep \
    --report-json var/bench/flat_sweep.json --report-md var/bench/flat_sweep.md
  ```

- **Legacy / helper:** `cargo run -p codec_compare -- --yuv clip.yuv --meta clip.json --out-json report.json --out-md report.md` (older harness; same optional **libx264** branch when FFmpeg is available).

- These outputs are **lab measurements** — do **not** treat them as proof SRSV2 “beats” another codec (**AVC** or **HEVC-class**) without your own methodology (`docs/srsv2_benchmarks.md`, `docs/hevc_competition_plan.md`).

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

Windows launcher modes match the Unix launcher: `player`, `server`, and `cli`, plus **`deps`** to print prerequisite status (Rust/cargo, rustfmt/clippy, MSVC, Git, FFmpeg, winget). Use **`deps -InstallDeps`** to install missing tools via winget where possible; add **`-InstallMsvc`** for Visual Studio Build Tools (large). **`-SkipDepsCheck`** skips checks for automation. The background server waits up to **`-ServerWaitSeconds`** (default **600**) for the first `cargo` compile; if startup fails, errors are in **`var\srs_license_server.stderr.log`** (script prints a tail). Linker **LNK1201** on `.pdb` under **`target\`** means the linker could not write that file (cloud sync on **Documents**, backup tools, another **cargo**/IDE handle, indexing, disk space, or a stale **`target`** tree—not only antivirus). Try **`cargo clean`**, build outside synced folders, or **`tools\run_windows.ps1 -DevLinkNoPdb`** to emit fewer PDBs in dev.

## Optional FFmpeg Compatibility

FFmpeg integration is isolated in `libsrs_compat` behind the `ffmpeg` feature:

```bash
cargo check -p libsrs_compat --features ffmpeg
```
