//! Shared lease for public runtime owners.

use std::sync::Arc;

use super::SupervisorCore;

/// One lease shared by the public `Supervisor` facade and every management handle.
///
/// Internal runtime tasks never clone this value. Its destructor therefore runs
/// when the last public owner disappears, even if listeners still hold internal
/// runtime components alive.
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
