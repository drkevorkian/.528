# Phase 1 bounded SQLite execution status

The bounded SQLite executor is now wired on `ai/phase0-baseline-security`.

## Current architecture

- `apps/srs_license_server/src/db_exec.rs` owns a bounded semaphore and runs synchronous rusqlite work through `tokio::task::spawn_blocking`.
- The permit limit is **1**, matching the server's current single `Mutex<Connection>` database architecture.
- `AppState::run_db` is the only request-path bridge used by the Axum handlers.
- Existing `Database` methods remain synchronous and continue to use parameterized `rusqlite::params!` SQL.
- No SQLite mutex is intentionally held across an `.await`.

The following request paths have been moved behind the bounded executor:

- authenticated issuance
- entitlement verification
- confirmation lookup and confirmation mutation
- client notification reads
- unsupported-playback reporting
- admin snapshots
- license/key/request/installation/audit mutations
- notification creation

## Important limitation

This branch has **not** been compiled or test-executed in the current AI execution environment. It is implementation-under-review, not merge-ready.

Required verification before merge:

```bash
cargo fmt --all --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo check -p libsrs_compat --features ffmpeg
```

Server-specific verification:

```bash
cargo test -p srs_license_server
cargo clippy -p srs_license_server --all-targets -- -D warnings
```

## Follow-up

SMTP delivery is synchronous and currently executes inside the same bounded blocking job used by notification creation. It no longer blocks Tokio's async runtime, but a slow SMTP server can occupy the single blocking permit and delay other DB-backed requests. Split SMTP onto its own bounded blocking resource before calling Phase 1 availability hardening complete.
