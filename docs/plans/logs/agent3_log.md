# Agent 3 Log

## 2026-04-27

### Docs

- Refreshed root `README` (`.528` / import / probe), `docs/specs/compatibility_layer.md`, `architecture_overview.md`, `container_format.md` (now an index to `528_container_format.md`), `528_container_format.md` (native audio `config`), `samples/README`, fuzz corpus README, and ADR-0002 consequences for probe/import behavior.

## 2026-04-19

### Changed

- Bootstrapped workspace root files and member crates/apps/tests/tools layout.
- Added `libsrs_contract`, `libsrs_compat`, and `libsrs_pipeline` integration layer.
- Added `apps/srs_cli` with native encode/decode/mux/demux/analyze/import/transcode wiring.
- Added `apps/srs_player` desktop UI shell (open/play/pause/stop/seek/status panels).
- Added `tests/e2e`, `tests/fuzz`, and `benchmarks` harness scaffolds.
- Added ADR/spec/plan files and integration docs.
- Replaced `libsrs_compat` synthetic-only ingest/probe paths with native `.528` / legacy `.srsm`, `.srsv`, and `.srsa` support and updated e2e coverage accordingly.

### Assumptions

- FFmpeg is optional at build time and isolated behind feature flag `ffmpeg`.
- Native codecs remain authoritative; compatibility layer is for foreign ingest/playback only.
- Initial player focuses on control-plane and metadata while decode/render loop matures.

### Interfaces depended on

- `libsrs_video` and `libsrs_audio` frame encode/decode APIs for native data paths.
- `libsrs_mux` and `libsrs_demux` for container read/write and seek behavior.
- Shared contract types from `libsrs_contract` and probing contracts from `libsrs_compat`.

### Tests added

- `tests/e2e/tests/basic_flow.rs` pipeline compatibility and native roundtrip tests.
- Fuzz target skeleton in `tests/fuzz/fuzz_targets/container_parser_demux_reader.rs`.
- Benchmark skeletons in `benchmarks/benches/encode_decode.rs` and `benchmarks/benches/seek_latency.rs`.

### Blockers

- FFmpeg backend currently feature-gated and minimal; full packet decode mapping remains future enhancement.
