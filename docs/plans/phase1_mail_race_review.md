# SMTP / outbox review (AI B)

Reviewed wired flow on `c239329` / current `main.rs`.

## What is correct

- `run_db` → enqueue queued → `run_mail` → `run_db` mark is the right domain split.
- Failed SMTP leaves `queued` today (good) except there is no claim, so two verifies can deliver twice.
- Confirmation token no longer in audit JSON.
- SQLite `Mutex<Connection>` already serializes verify inserts; the remaining race is **after** commit, in the HTTP handler mail step.

## Defects

1. **Double SMTP on concurrent verify**
   `verify_json` always calls `pending_mail_for_device` after `verify_key`. Two in-flight verifies for the same new device both see `queued` and both call `deliver_email`. Fix: `outbox_ops::claim_queued` before SMTP; loser skips mail.

2. **`pending_mail_for_device` is under-scoped**
   Filter is only `device_install_id`. That id is client-supplied and not unique per license. Query can attach the newest queued confirmation for *any* license sharing that string. Must also filter `v.license_id` from the just-computed entitlement, plus `approved_at IS NULL`, unexpired, `record_state = active`. SQL is in `outbox_ops::PENDING_MAIL_SQL`.

3. **Two UPDATEs for one success**
   `mark_notification_sent` then `mark_notification_delivered` can leave `sent` without `delivered` if the process dies between them. Use `outbox_ops::mark_accepted`.

4. **Retry policy**
   Confirmation retry happens only when that device verifies again while the row is still `queued`. Admin notifications that fail SMTP never retry unless the operator creates another notification. Acceptable for Phase 1 if documented. Do **not** add an unbounded background poller on the DB permit.
   After claim, Failed must `unclaim_to_queued` or the row sticks in `sending` forever.

5. **Confirmation mail body still embeds `request.key`**
   That is the raw license credential. Token is already in the confirm URL. A should drop the raw key from the body (key_id / last-4 hint only). Not a route change.

6. **DB fairness**
   Permit=1 + fat `admin_snapshot` means one dashboard refresh holds the only DB permit for a large query set. Mail is already off that permit. Do not raise DB permits above 1 while there is a single connection. Snapshot slimming is later work, not Block 7.

## Handler shape after fix

```text
run_db { verify_key; select pending mail FOR this license+device }
claim_queued in run_db (or same closure)
if claimed:
  run_mail deliver
  run_db mark_accepted | unclaim_to_queued
```

Do not merge until claim + license-scoped select are in `verify_json` / `create_notification_json`.
Codec still frozen. Wait for CI on the draft PR.


## Delivery guarantee

The outbox is intentionally **at-least-once**, not exactly-once.

A process crash after the SMTP server accepts a message but before `mark_accepted` commits can leave the row in the internal `sending` state. Server startup resets stale `sending` rows to `queued` so delivery is not permanently lost. A subsequent retry can therefore duplicate that message.

Exactly-once SMTP delivery cannot be guaranteed by this local database alone because SMTP acceptance and the SQLite status update are not one atomic transaction. The Phase 1 priority is:

- never lose a queued notification silently;
- prevent ordinary concurrent double-send races with atomic claiming;
- recover stale claims after restart;
- accept the narrow crash-window duplicate risk.

If a future mail provider supports a durable external idempotency key, that can tighten the guarantee without coupling SMTP I/O to the SQLite transaction.
