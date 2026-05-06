# Windows development baseline

_Block 0 verification — 2026-05-04. Repository: `https://github.com/drkevorkian/.528`._

## Environment

| Item | Value |
|------|--------|
| **Windows** | Microsoft Windows NT 10.0.26200.0 (Windows 10 Pro) |
| **rustc** | rustc 1.94.1 (e408947bf 2026-03-25) |
| **cargo** | cargo 1.94.1 (29ea6fb6a 2026-03-24) |
| **rustfmt** | rustfmt 1.8.0-stable (e408947bfd 2026-03-25) |
| **clippy** | clippy 0.1.94 (e408947bfd 2026-03-25) |
| **git** | git version 2.53.0.windows.2 |
| **FFmpeg** | Available on PATH: `C:\Users\owner\AppData\Local\Microsoft\WinGet\Links\ffmpeg.exe` — `ffmpeg version 8.1-full_build-www.gyan.dev` (first line of `ffmpeg -version`) |
| **MSVC Build Tools** | **Detected OK** — `tools\run_windows.ps1 deps` reported: `MSVC C++ tools OK (VS / Build Tools)` |

## Commands run

Executed from the repo root `C:\Users\owner\Documents\GitHub\.528`:

```text
powershell -ExecutionPolicy Bypass -File tools\run_windows.ps1 deps
cargo fmt --all --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

`tools\run_windows.ps1 deps` also confirmed: cargo, rustc, rustup, rustfmt, clippy, git, and optional FFmpeg checks **OK**.

## Pass / fail summary

| Step | Result |
|------|--------|
| `run_windows.ps1 deps` | **PASS** (exit code 0) |
| `cargo fmt --all --check` | **PASS** after remediation (see below) |
| `cargo check --workspace` | **PASS** (exit code 0) |
| `cargo test --workspace` | **PASS** (exit code 0) |
| `cargo clippy --workspace --all-targets -- -D warnings` | **PASS** after remediation (see below) |

No PDB or linker errors occurred; `cargo clean` and `-DevLinkNoPdb` were **not** required on this machine.

## Failures encountered (exact output) and remediation

These blocked baseline verification until fixed. **No codec algorithm changes** were made; fixes were merge-conflict repair and Clippy lint attributes only.

### 1. Initial `cargo fmt --all --check` — FAIL (parse error)

```text
error: this file contains an unclosed delimiter
    --> \\?\C:\Users\owner\Documents\GitHub\.528\tools\quality_metrics\src\srsv2_progress_report.rs:1297:3
     |
 961 | mod tests {
     |           - unclosed delimiter
...
1297 | }
     | -^
     | |
     | ...as it matches this but it has different indentation

Error writing files: failed to resolve mod `srsv2_progress_report`: cannot parse \\?\C:\Users\owner\Documents\GitHub\.528\tools\quality_metrics\src\srsv2_progress_report.rs
```

**Cause:** Unresolved Git merge conflict delimiter lines (HEAD / separator / incoming branch) were left in `tools/quality_metrics/src/srsv2_progress_report.rs`.

**Remediation:** Conflict markers removed; `read_json`, `build_progress_report_strict`, `build_progress_report`, and the `mod tests` section were merged into valid Rust. `cargo fmt --all --check` then **PASS**.

### 2. Initial `cargo clippy ... -D warnings` — FAIL (`clippy::double_must_use`)

```text
error: this function has a `#[must_use]` attribute with no message, but returns a type already marked as `#[must_use]`
   --> crates\libsrs_video\src\srsv2\rdo.rs:187:1
...
   = note: `-D clippy::double-must-use` implied by `-D warnings`

error: this function has a `#[must_use]` attribute with no message, but returns a type already marked as `#[must_use]`
   --> crates\libsrs_video\src\srsv2\rdo.rs:222:1
```

**Remediation:** Removed redundant `#[must_use]` on `autofast_partition_mb_wire_cost` and `autofast_partition_mb_rdo_score` (both return `Result<...>`, which is already `#[must_use]`). Behavior unchanged. Clippy then **PASS**.

## Workspace status (acceptance)

- **Check / test / Clippy status:** Known and green after the steps above.
- **Codec logic:** Unchanged except for removal of redundant attributes on two public `Result`-returning helpers in `rdo.rs` (lint-only).
- **This document:** Present at `docs/windows_dev_baseline.md`.
