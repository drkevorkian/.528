# ADR-0005: Quality and Testing

## Status
Accepted

## Decision
Adopt layered quality gates: unit tests in crates, integration tests in `tests/e2e` and app crates, fuzz targets in `tests/fuzz`, criterion benchmarks in `benchmarks`, and CI coverage for desktop, licensing, and server workflows.

## Consequences
- early enforcement of integration compatibility
- clear extension points for regression, fuzzing, and performance baselines
- desktop licensing fallback and editor gating must be exercised in tests
- server-side key issuance, verification, confirmation, and rollover flows belong in automated tests
- CI should run workspace tests, fuzz crate checks, and benchmark builds as release hardening gates
