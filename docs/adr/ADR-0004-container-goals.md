# ADR-0004: Container Goals

## Status
Accepted

## Decision
Container demux/mux internals remain native and modular, while container probing/import for broad compatibility can use optional compatibility backends.

## Goals
- clean boundary between ingest and native container logic
- support future incremental migration from compat sources to native demux
