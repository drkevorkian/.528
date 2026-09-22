# SMTP isolation (AI B)

Problem: `create_and_send_notification` and `verify_key` call `deliver_email` while still conceptually inside the single DB executor job, so a hung SMTP server stalls every license request.

## Required handler shape

```text
run_db  { enqueue outbox row as queued + audit; commit; drop mutex }
run_mail { deliver_email(...) }     // BoundedMailExecutor, limit 2
run_db  { if Delivered | LoggedOnly -> mark sent+delivered; if Failed -> leave queued }
```

Do **not** mark `sent` before SMTP returns. Failed delivery must leave `notification_state = queued`.

Call sites to split:
- `Database::create_and_send_notification`
- `Database::verify_key` confirmation-mail tail

`deliver_email` moves to `mailer.rs`. Logs must not contain SMTP password, full recipient local-part secrets, or message body (confirmation tokens live there).

`db_exec.rs` needs `BoundedDbExecutor::with_limit` so mail can share the type without sharing the permit pool.

Tests: `mail_exec` inflight cap; `mailer::redact_addr`.

Do not merge until A wires `mod mail_exec; mod mailer;` and the two call sites. Codec untouched.
