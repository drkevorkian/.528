//! Bounded SQLite execution for Axum.
//!
//! Tokio async worker threads must not run rusqlite disk I/O. Every transaction
//! runs inside `spawn_blocking` after acquiring one of a fixed number of permits
//! so a flood of HTTP requests cannot create an unbounded blocking-thread pool.
//!
//! Prepared statements stay in the existing `Database` methods; this module only
//! moves those closures onto the blocking pool.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Maximum concurrent rusqlite jobs. One connection + one writer today.
pub const DB_PERMIT_LIMIT: usize = 1;

#[derive(Clone, Debug)]
pub struct BoundedDbExecutor {
    permits: Arc<Semaphore>,
}

impl BoundedDbExecutor {
    pub fn new() -> Self {
        Self {
            permits: Arc::new(Semaphore::new(DB_PERMIT_LIMIT)),
        }
    }

    pub fn available_permits(&self) -> usize {
        self.permits.available_permits()
    }

    /// Run `work` on a blocking thread after taking a permit.
    ///
    /// The permit is held until `work` returns so a second caller waits rather
    /// than spawning another blocking task.
    pub async fn run<T, F>(&self, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        let permit: OwnedSemaphorePermit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("database executor semaphore closed"))?;
        let joined = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work()
        })
        .await
        .map_err(|err| anyhow!("database worker join error: {err}"))?;
        joined
    }
}

impl Default for BoundedDbExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn run_executes_work_and_returns_value() {
        let exec = BoundedDbExecutor::new();
        let value = exec.run(|| Ok(7_u32)).await.expect("run");
        assert_eq!(value, 7);
        assert_eq!(exec.available_permits(), DB_PERMIT_LIMIT);
    }

    #[tokio::test]
    async fn run_propagates_work_errors() {
        let exec = BoundedDbExecutor::new();
        let err = exec
            .run(|| -> Result<()> { Err(anyhow!("constraint failed")) })
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("constraint failed"));
        assert_eq!(exec.available_permits(), DB_PERMIT_LIMIT);
    }

    #[tokio::test]
    async fn permit_serializes_concurrent_jobs() {
        let exec = BoundedDbExecutor::new();
        let inflight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut joins = Vec::new();
        for _ in 0..4 {
            let exec = exec.clone();
            let inflight = inflight.clone();
            let peak = peak.clone();
            joins.push(tokio::spawn(async move {
                exec.run(move || {
                    let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    inflight.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
                .await
            }));
        }
        for join in joins {
            join.await.expect("task").expect("db job");
        }
        assert_eq!(peak.load(Ordering::SeqCst), 1, "permit limit is 1");
        assert_eq!(exec.available_permits(), DB_PERMIT_LIMIT);
    }
}
