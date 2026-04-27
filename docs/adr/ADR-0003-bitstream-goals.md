# ADR-0003: Bitstream Goals

## Status
Accepted

## Decision
Define stable packet and frame metadata contracts first, then wire codec-specific parsing through native crates.

## Goals
- deterministic timestamp handling via explicit timebase
- strict typed stream and track IDs
- format-agnostic packet contract for early integration
