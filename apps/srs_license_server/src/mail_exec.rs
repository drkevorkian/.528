//! Bounded SMTP / blocking-mail execution.
//!
//! Mail delivery must not hold the SQLite executor permit. Use this pool for
//! `lettre` SMTP (and the log-fallback path) so a slow mail server cannot stall
//! license verify/issue.
//!
//! Permit count is small and fixed. Do not `spawn_blocking` mail work from
//! handlers except through this type.

use crate::db_exec::BoundedDbExecutor;

/// Concurrent SMTP jobs. Keep this tiny; SMTP is latency-heavy.
pub const MAIL_PERMIT_LIMIT: usize = 2;

/// Same permit/spawn_blocking machinery as the DB executor, different pool.
pub type BoundedMailExecutor = BoundedDbExecutor;

pub fn new_mail_executor() -> BoundedMailExecutor {
    BoundedDbExecutor::with_limit(MAIL_PERMIT_LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn mail_pool_is_independent_capacity() {
        let exec = new_mail_executor();
        assert_eq!(exec.available_permits(), MAIL_PERMIT_LIMIT);
        exec.run(|| Ok(())).await.expect("run");
        assert_eq!(exec.available_permits(), MAIL_PERMIT_LIMIT);
    }

    #[tokio::test]
    async fn mail_pool_caps_inflight() {
        let exec = new_mail_executor();
        let inflight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut joins = Vec::new();
        for _ in 0..6 {
            let exec = exec.clone();
            let inflight = inflight.clone();
            let peak = peak.clone();
            joins.push(tokio::spawn(async move {
                exec.run(move || {
                    let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(15));
                    inflight.fetch_sub(1, Ordering::SeqCst);
                    Ok::<(), anyhow::Error>(())
                })
                .await
            }));
        }
        for join in joins {
            join.await.expect("task").expect("mail job");
        }
        assert!(peak.load(Ordering::SeqCst) <= MAIL_PERMIT_LIMIT);
    }
}
