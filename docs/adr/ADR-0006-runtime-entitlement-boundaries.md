# ADR-0006: Runtime Entitlement Boundaries

## Status
Accepted

## Context

The project is adding a same-repo licensing service, website, and dual-mode desktop
experience. The media crates remain source-visible and focused on codec/container work,
while editor capabilities should be unlocked only for verified users of official builds.

## Decision

Keep entitlement enforcement at the application and shared service layers.

- `srs_player` and `srs_cli` enforce feature availability using verified entitlements.
- Shared application services may expose editor-only operations behind entitlement-aware wrappers.
- `libsrs_pipeline`, `libsrs_compat`, and the codec/container crates remain free of licensing business logic.
- The client trusts only signed entitlements issued by the licensing server.
- When verification is unavailable or pending, applications fall back to `basic` / play-only behavior.

## Consequences

- Official builds can enforce product policy without coupling entitlements to file formats or codec internals.
- The system is not tamper-proof DRM against modified source builds because the codebase remains source-visible.
- Server-side feature assignment, audit logging, and key rotation become the primary control plane.
- Tests must cover fallback-to-basic behavior, entitlement verification, and gated command/UI paths.
