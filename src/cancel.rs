//! Cancellation tokens and registry.
//!
//! The UI thread sets a `CancellationToken` directly when the user clicks
//! cancel, rather than queuing a message behind the blocked worker. The
//! pipeline checks the token before and after each stage and after every
//! long blocking call (download, ffmpeg, docker).
//!
//! `CancellationToken` is `Clone` (cheap `Arc<AtomicBool>` share) and `Send`,
//! so it can be passed into the pipeline and subprocess layers without
//! coupling them to the worker loop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use uuid::Uuid;

/// A cooperative cancellation flag, cheap to clone and share across threads.
#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    /// Create a fresh, un-cancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Signal cancellation. Idempotent.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// True if `cancel` has been called.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Return `Err(Cancelled)` if already cancelled, otherwise `Ok(())`.
    pub fn check(&self) -> Result<(), CancelledError> {
        if self.is_cancelled() {
            Err(CancelledError)
        } else {
            Ok(())
        }
    }
}

impl std::fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

/// Error returned by [`CancellationToken::check`] when cancellation was
/// requested.
#[derive(Debug, Clone, thiserror::Error)]
#[error("cancelled")]
pub struct CancelledError;

impl From<CancelledError> for crate::pipeline::PipelineError {
    fn from(_: CancelledError) -> Self {
        crate::pipeline::PipelineError::Cancelled
    }
}

/// Maps a job id to its cancellation token so the UI thread can cancel a
/// running job without going through the worker's message channel.
#[derive(Debug, Default)]
pub struct CancellationRegistry {
    tokens: Mutex<HashMap<Uuid, CancellationToken>>,
}

impl CancellationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a fresh token for `job_id` and return a clone of it. If a
    /// token already exists (shouldn't normally happen), it is replaced.
    pub fn register(&self, job_id: Uuid) -> CancellationToken {
        let token = CancellationToken::new();
        self.tokens.lock().unwrap().insert(job_id, token.clone());
        token
    }

    /// Look up the token for `job_id`, if any.
    pub fn get(&self, job_id: Uuid) -> Option<CancellationToken> {
        self.tokens.lock().unwrap().get(&job_id).cloned()
    }

    /// Remove the token for `job_id` (called after the job's pipeline has
    /// exited and the job is persisted as Cancelled).
    pub fn remove(&self, job_id: Uuid) {
        self.tokens.lock().unwrap().remove(&job_id);
    }

    /// Signal cancellation for `job_id` if a token exists. Returns true if a
    /// token was found and cancelled.
    pub fn cancel(&self, job_id: Uuid) -> bool {
        if let Some(token) = self.get(job_id) {
            token.cancel();
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_starts_uncancelled() {
        let t = CancellationToken::new();
        assert!(!t.is_cancelled());
        assert!(t.check().is_ok());
    }

    #[test]
    fn cancel_sets_flag() {
        let t = CancellationToken::new();
        t.cancel();
        assert!(t.is_cancelled());
        assert!(t.check().is_err());
    }

    #[test]
    fn cloned_token_shares_state() {
        let t = CancellationToken::new();
        let t2 = t.clone();
        t.cancel();
        assert!(t2.is_cancelled());
    }

    #[test]
    fn registry_register_get_remove() {
        let reg = CancellationRegistry::new();
        let id = Uuid::new_v4();
        let token = reg.register(id);
        assert!(reg.get(id).is_some());
        // The returned token shares state with the one in the registry.
        token.cancel();
        assert!(reg.get(id).unwrap().is_cancelled());
        reg.remove(id);
        assert!(reg.get(id).is_none());
    }

    #[test]
    fn registry_cancel_returns_false_for_unknown() {
        let reg = CancellationRegistry::new();
        assert!(!reg.cancel(Uuid::new_v4()));
    }

    #[test]
    fn registry_cancel_signals_token() {
        let reg = CancellationRegistry::new();
        let id = Uuid::new_v4();
        let token = reg.register(id);
        assert!(reg.cancel(id));
        assert!(token.is_cancelled());
    }
}
