# ADR-0001: Language and Workspace

## Status
Accepted

## Decision
Use Rust with a cargo workspace split by responsibility (contract, compatibility, pipeline, applications, tests, benchmarks).

## Rationale
Rust provides memory safety and strong performance characteristics while enabling independent crate evolution.
