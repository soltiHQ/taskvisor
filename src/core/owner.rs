//! Tracks the lifetime of public runtime owners.

use std::sync::Arc;

use super::SupervisorCore;

/// Shared ownership held by `Supervisor` and every management handle.
///
/// Internal workers do not hold this value.
/// Its destructor therefore runs when the last public owner disappears and starts best-effort runtime cancellation.
pub(crate) struct RuntimeOwner {
    core: Arc<SupervisorCore>,
}

impl RuntimeOwner {
    pub(crate) fn new(core: Arc<SupervisorCore>) -> Arc<Self> {
        Arc::new(Self { core })
    }

    pub(crate) fn core(&self) -> &Arc<SupervisorCore> {
        &self.core
    }
}

impl std::fmt::Debug for RuntimeOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeOwner").finish_non_exhaustive()
    }
}

impl Drop for RuntimeOwner {
    fn drop(&mut self) {
        self.core.abandon();
    }
}
