//! Thread-safe progress and cooperative cancellation for long operations.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("operation cancelled")
    }
}

impl std::error::Error for Cancelled {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressEvent {
    pub phase: &'static str,
    pub completed: u64,
    pub total: u64,
}

/// A token that may be cancelled from another thread.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }
}

type Callback = dyn Fn(ProgressEvent) + Send + Sync;

/// Share one instance across Rayon workers. Callbacks may run concurrently;
/// `completed` is assigned atomically and reaches `total` on success.
#[derive(Clone, Default)]
pub struct Progress {
    token: CancelToken,
    completed: Arc<AtomicU64>,
    callback: Option<Arc<Callback>>,
}

impl std::fmt::Debug for Progress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Progress")
            .field("completed", &self.completed.load(Ordering::Acquire))
            .field("cancelled", &self.token.is_cancelled())
            .finish()
    }
}

impl Progress {
    pub fn new(
        token: CancelToken,
        callback: impl Fn(ProgressEvent) + Send + Sync + 'static,
    ) -> Self {
        Self {
            token,
            completed: Arc::new(AtomicU64::new(0)),
            callback: Some(Arc::new(callback)),
        }
    }

    pub fn token(&self) -> CancelToken {
        self.token.clone()
    }

    pub fn check(&self) -> Result<(), Cancelled> {
        self.token.check()
    }

    pub fn report(&self, phase: &'static str, completed: u64, total: u64) -> Result<(), Cancelled> {
        self.check()?;
        if let Some(callback) = &self.callback {
            callback(ProgressEvent {
                phase,
                completed,
                total,
            });
        }
        self.check()
    }

    /// Reports a committed operation. Cancellation requested by this callback
    /// cannot roll back an already published file or loaded document.
    pub fn finish(&self, phase: &'static str, total: u64) {
        if let Some(callback) = &self.callback {
            callback(ProgressEvent {
                phase,
                completed: total,
                total,
            });
        }
    }

    pub fn step(&self, phase: &'static str, total: u64) -> Result<(), Cancelled> {
        let completed = self.completed.fetch_add(1, Ordering::AcqRel) + 1;
        self.report(phase, completed, total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn workers_report_and_cancellation_is_shared() {
        let token = CancelToken::default();
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let progress = Progress::new(token.clone(), move |event| {
            captured.lock().unwrap().push(event)
        });
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let progress = progress.clone();
                std::thread::spawn(move || progress.step("island", 4))
            })
            .collect();
        for worker in workers {
            worker.join().unwrap().unwrap();
        }
        let mut completed: Vec<u64> = events.lock().unwrap().iter().map(|e| e.completed).collect();
        completed.sort_unstable();
        assert_eq!(completed, [1, 2, 3, 4]);
        token.cancel();
        assert_eq!(progress.step("island", 4), Err(Cancelled));
        assert_eq!(events.lock().unwrap().len(), 4);
    }

    #[test]
    fn callback_can_cancel_operation() {
        let token = CancelToken::default();
        let captured = token.clone();
        let progress = Progress::new(token, move |_| captured.cancel());
        assert_eq!(progress.report("pack", 0, 1), Err(Cancelled));
    }
}
