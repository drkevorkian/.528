# Licensing and Editions Spec

## Goals

- Keep one desktop binary with a safe `basic` playback default.
- Unlock richer editor capabilities through server-issued entitlements.
- Keep licensing policy above the media codec/container crates.
- Allow local development against `localhost`, with `127.0.0.1` as a backup endpoint.

## Components

- `libsrs_licensing_proto`: shared request/response DTOs and signed entitlement envelope.
- `libsrs_licensing_client`: desktop/CLI verification, caching, and fallback logic.
- `srs_license_server`: key issuance, verification, confirmation workflow, and website.
- `srs_player`: dual-workspace desktop shell.
- `srs_cli`: command-line surface that must respect the same editor entitlement rules.

## Editions

### Basic

- playback-oriented controls
- metadata inspection
- non-destructive read-only workflows

### Editor

- editor workspace visibility
- encode/decode
- compress/export
- mux/demux/import/transcode
- selection/timeline/frame operations

Feature availability is stored server-side per license key and delivered to the client
inside a signed entitlement envelope.

## Key And Verification Flow

1. A user visits the website and requests a key.
2. The website issues a fresh key, stores the owner email, and records initial web registration metadata.
3. The desktop or CLI submits:
   - key
   - best-effort client IP
   - OS information
   - stable installation identifier
   - app version
4. The server validates the key and returns a signed entitlement envelope.
5. The client verifies the signature locally before enabling editor capabilities.

## New Installation / New IP Flow

- If a known trusted installation presents the key, the server returns the assigned features.
- If a new installation or materially new origin presents the key, the server creates a pending confirmation request.
- The server sends a "Was this you?" email to the original owner email address.
- While confirmation is pending, the entitlement status stays in a non-editor state and clients fall back to `basic`.
- If the request is confirmed, the installation becomes trusted.
- If the request is not confirmed within 72 hours, the server may issue a replacement key for the requesting installation, record the event in audit history, and keep the new installation logically separate from the original owner.

## Trust Model

- The server is the source of truth for key ownership and feature assignment.
- The client trusts only signed entitlement envelopes, not plain JSON flags.
- Transport security should be HTTPS in production.
- IP and OS info are logged for audit and confirmation flow, but IP alone is not treated as identity.

## Fallback Behavior

- If the licensing server is unreachable, the desktop and CLI fall back to `basic` behavior.
- Cached entitlements may be used only if they are signed and unexpired.
- Pending, revoked, or expired entitlements must not unlock editor capabilities.

## Configuration

Client and server configuration should remain externalized.

Default development behavior:

- primary licensing URL: `http://localhost:3000`
- backup licensing URL: `http://127.0.0.1:3000`

Production deployments must override:

- URLs
- signing material
- database settings
- mail transport settings

## Data To Persist

The licensing service persists:

- licenses and key versions
- feature assignments
- installation records
- verification requests
- audit events
- owner email and notification metadata
