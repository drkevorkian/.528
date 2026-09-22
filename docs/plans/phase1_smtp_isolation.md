# Phase 1 SMTP isolation status

SMTP delivery is now isolated from the SQLite executor on `ai/phase0-baseline-security`.

## Current execution model

```text
run_db
  -> enqueue outbox row as queued
  -> write audit record
  -> commit

run_mail
  -> bounded blocking mail pool (2 permits)
  -> SMTP or redacted local-log fallback

run_db
  -> on Delivered / LoggedOnly: mark sent + delivered
  -> on Failed: leave queued for retry
```

The DB executor and mail executor use separate semaphore pools. A slow SMTP server therefore does not consume the single SQLite execution permit.

## Security properties

- SMTP passwords are not logged.
- Confirmation-message bodies are not logged.
- Confirmation URLs/tokens are not written to audit payloads.
- Recipient logging redacts the local part.
- SMTP protocol errors are reduced to non-sensitive status logging.
- Failed SMTP delivery leaves the notification queued instead of falsely marking it sent.
- Admin snapshots expose notification recipient/subject/state but not body content.

## Confirmation flow

Verification creates a queued confirmation email. The client response reports that confirmation mail was **queued**, not necessarily delivered.

The confirmation endpoint is split:

- `GET /confirm/{token}`: read-only preview for an active, unexpired capability token.
- `POST /confirm/{token}`: performs the approval mutation.

The preview response uses `Cache-Control: no-store`, `Referrer-Policy: no-referrer`, and a restrictive CSP with `form-action 'self'`.

The unguessable confirmation token is itself the one-time capability required for POST; this flow does not rely on an ambient browser administrator cookie.

## Verification still required

The branch must pass CI / local verification before merge:

```bash
cargo fmt --all --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo check -p libsrs_compat --features ffmpeg
```

Do not merge this phase based on static review alone.
