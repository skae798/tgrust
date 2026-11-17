use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::task::JoinHandle;

/// Owns the sender task, including its session-file lock. A detached sender must
/// never outlive a cancelled connection attempt or its last Telegram owner.
pub struct SessionRunner {
    task: JoinHandle<()>,
}

impl SessionRunner {
    pub fn spawn(task: impl Future<Output = ()> + Send + 'static) -> Self {
        Self {
            task: tokio::spawn(task),
        }
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    pub async fn finish_startup<T>(
        mut self,
        timeout: Duration,
        handshake: impl Future<Output = Result<T>>,
    ) -> Result<(Self, T)> {
        let result = tokio::time::timeout(timeout, handshake)
            .await
            .context("Telegram не ответил вовремя")
            .and_then(|result| result);
        match result {
            Ok(value) => Ok((self, value)),
            Err(error) => {
                // A retry can start immediately after this error. Wait until the
                // task has released the lock, not merely until abort is requested.
                self.task.abort();
                let _ = (&mut self.task).await;
                Err(error)
            }
        }
    }
}

impl Drop for SessionRunner {
    fn drop(&mut self) {
        self.task.abort();
    }
}
