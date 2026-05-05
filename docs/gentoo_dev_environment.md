# Gentoo development environment (Reality Sheet)

## Purpose

Establish a reproducible **Gentoo Linux** baseline for this Rust workspace: toolchain, optional native deps, launchers, and verification commands. **Block 0** is environment and tooling only — **no codec algorithm changes**.

Workspace facts:

- **MSRV:** `1.78` (`rust-version` in root `Cargo.toml`).
- **Toolchain file:** `rust-toolchain.toml` pins **`stable`** with components **`rustfmt`** and **`clippy`** (rustup uses these when present).

## Required Gentoo packages (Portage)

Install with **`emerge`** as needed; nothing in this repo auto-installs packages.

| Role | Typical atoms | Notes |
|------|----------------|--------|
| Rust toolchain | `dev-lang/rust` | Prefer Portage unless **`rustup`** already manages `/usr/bin/rustc` (avoid mixing blindly). |
| rustfmt + clippy | USE on `dev-lang/rust`: **`rustfmt`**, **`clippy`** | Must satisfy `rust-toolchain.toml` component expectations; missing USE yields **`no such command: clippy`** from Cargo and **`FAIL: cargo clippy`** in `tools/gentoo_dev_check.sh`. Example: `emerge -av dev-lang/rust` with **`USE="clippy rustfmt"`** (exact flags depend on your profile). |
| pkg-config | `virtual/pkgconfig` | Native crates probe libraries via **`pkg-config`** at build time. |
| Git | `dev-vcs/git` | Required for development and many tooling workflows. |

Check versions anytime:

```bash
rustc --version
cargo --version
rustfmt --version
cargo clippy --version
git --version
```

### Optional: FFmpeg

| Atom | When you need it |
|------|------------------|
| `media-video/ffmpeg` | **`ffmpeg`** on `PATH` for optional **`bench_srsv2 --compare-x264`**, README x264 comparison snippets, and **`cargo check -p libsrs_compat --features ffmpeg`**. |

Core SRSV2 benches and default **`cargo check --workspace`** do **not** require FFmpeg.

### GUI / OpenGL (eframe / egui / winit)

Desktop apps (`srs_player`, `srs_admin`) use **eframe**. Gentoo does not use one fixed “dev package” name for every transitive GL/X11/Wayland dependency. If **`cargo build`** fails at link time, install the **libraries** the linker names (often `x11-libs/libX11`, Mesa/Vulkan stacks, etc., depending on features). Prefer fixing **reported** missing `.so` dependencies over pre-installing a huge meta-set.

## X11 vs Wayland (Cursor / GUI)

- **`tools/run_gentoo.sh`** sets **`SRS_GUI_BACKEND=x11`** by default to reduce known **Wayland + winit** pointer crashes on some setups (see root `README.md`).
- Override when appropriate: `SRS_GUI_BACKEND=wayland bash tools/run_gentoo.sh` or `bash tools/run_unix.sh --gui-backend wayland`.
- **Cursor** follows your session; check `echo "$XDG_SESSION_TYPE"` (**wayland** vs **x11**). For **this repo’s** GUI binaries, defaulting to **X11** (`SRS_GUI_BACKEND=x11`) matches the Gentoo wrapper unless you need native Wayland.

Environment variables used by `tools/run_unix.sh` for GUI backends include **`DISPLAY`** (X11), **`WAYLAND_DISPLAY`** (Wayland), and **`WINIT_UNIX_BACKEND`** (`x11` / `wayland`).

## Exact Cargo commands (verification)

Run from the repository root:

```bash
cargo fmt --all --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Optional when FFmpeg is installed:

```bash
cargo check -p libsrs_compat --features ffmpeg
```

## Exact launcher and check commands

Environment / prerequisite probe (no installs, no root):

```bash
bash tools/gentoo_dev_check.sh
```

Application launchers (see `README.md`):

```bash
bash tools/run_gentoo.sh
bash tools/run_unix.sh
bash tools/run_unix.sh server
bash tools/run_unix.sh cli analyze path/to/file.528
bash tools/run_unix.sh --gui-backend wayland
bash tools/run_unix.sh --config /path/to/srs.toml
```

Useful env vars: **`SRS_GUI_BACKEND`**, **`SRS_OPEN_ADMIN_UI`**, **`SRS_CONFIG_PATH`** (see `tools/run_unix.sh`).

## Where logs go

| Log file | Producer |
|----------|-----------|
| `var/srs_license_server.log` | Local licensing server background process (`tools/run_unix.sh`) |
| `var/srs_admin.log` | Admin UI when enabled |

The `var/` directory is tracked with **`var/.gitkeep`** only; runtime contents are listed in **`.gitignore`** (`var/*`). Create `var/` automatically when using launchers.

## Known Gentoo pitfalls

- **Missing rustfmt/clippy** on system Rust: enable **`rustfmt`** / **`clippy`** USE flags on **`dev-lang/rust`**, or use **rustup** with `rust-toolchain.toml`.
- **Mixing Portage Rust and rustup:** two toolchains on `PATH` can disagree; pick one primary **`rustc`** / **`cargo`** and verify with **`which rustc`**.
- **USE flags:** Gentoo `dev-lang/rust` without **`llvm`** / **`rustfmt`** / **`clippy`** as needed breaks CI-like checks.
- **Wayland + winit:** pointer/input crashes reported upstream for some stacks — use **`SRS_GUI_BACKEND=x11`** when stable GUI matters.
- **FFmpeg / `ffmpeg-next`:** optional feature needs development headers from **`media-video/ffmpeg`** (with typical `USE` for bundled libs as you prefer).
- **`target/` disk usage:** build artifacts grow large; **`target/`** is gitignored — ensure sufficient disk space on **`TMPDIR`** / repo partition.

## Host snapshot (optional)

Fill after running diagnostics on your machine:

```bash
uname -a
echo "$XDG_SESSION_TYPE"
ffmpeg -version 2>/dev/null || true
pkg-config --version 2>/dev/null || true
```
