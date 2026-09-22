//! Outbox status transitions that must stay on the DB executor.
//!
//! Callers: claim a queued row before SMTP, mark accepted in one statement after
//! success, and unclaim back to queued on SMTP failure so a later verify can retry.

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection};

/// Atomically take a queued row so two concurrent verifies cannot SMTP the same mail.
/// Returns false if another worker already claimed it or it is no longer queued.
pub fn claim_queued(conn: &Connection, email_id: &str, now_epoch_s: i64) -> Result<bool> {
    let n = conn.execute(
        "UPDATE email_outbox
         SET notification_state = 'sending',
             state_changed_at_epoch_s = ?1
         WHERE email_id = ?2
           AND notification_state = 'queued'
           AND record_state = 'active'",
        params![now_epoch_s, email_id],
    )?;
    Ok(n == 1)
}

/// Single-statement sent+delivered. Do not call mark_sent then mark_delivered.
pub fn mark_accepted(conn: &Connection, email_id: &str, now_epoch_s: i64) -> Result<()> {
    let n = conn.execute(
        "UPDATE email_outbox
         SET sent_at_epoch_s = COALESCE(sent_at_epoch_s, ?1),
             delivered_at_epoch_s = COALESCE(delivered_at_epoch_s, ?1),
             notification_state = 'delivered',
             state_changed_at_epoch_s = ?1
         WHERE email_id = ?2
           AND record_state = 'active'",
        params![now_epoch_s, email_id],
    )?;
    if n != 1 {
        return Err(anyhow!("outbox row missing while marking delivered"));
    }
    Ok(())
}

/// SMTP failed after claim. Leave retryable without pretending it was sent.
pub fn unclaim_to_queued(conn: &Connection, email_id: &str, now_epoch_s: i64) -> Result<()> {
    conn.execute(
        "UPDATE email_outbox
         SET notification_state = 'queued',
             state_changed_at_epoch_s = ?1
         WHERE email_id = ?2
           AND notification_state = 'sending'
           AND record_state = 'active'",
        params![now_epoch_s, email_id],
    )?;
    Ok(())
}

/// Safer selector than device-id-only. Scope to this license and live requests.
pub const PENDING_MAIL_SQL: &str = "
SELECT e.email_id, e.recipient, e.subject, e.body
FROM email_outbox e
JOIN verification_requests v ON v.request_id = e.request_id
WHERE v.device_install_id = ?1
  AND v.license_id = ?2
  AND v.approved_at_epoch_s IS NULL
  AND v.expires_at_epoch_s >= ?3
  AND v.record_state = 'active'
  AND e.notification_state = 'queued'
  AND e.record_state = 'active'
ORDER BY e.created_at_epoch_s DESC
LIMIT 1
";
