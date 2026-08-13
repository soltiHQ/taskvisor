//! Exposes registry-backed task state to runtime callers without the command queue.
//!
//! [`SupervisorHandle`](crate::SupervisorHandle) uses these reads for membership
//! and current-attempt queries. Membership comes from the registry. Physical
//! activity combines registry activity bits with attempts still retained by
//! the reaper after membership ends.

use std::sync::Arc;

use super::super::SupervisorCore;
use crate::identity::TaskId;

impl SupervisorCore {
    /// Returns registry members and entries still completing removal.
    ///
    /// Results are `(id, label)` pairs sorted by identity.
    pub(in crate::core) async fn list_tasks(&self) -> Vec<(TaskId, Arc<str>)> {
        self.registry.list().await
    }

    /// Returns sorted labels that still own a physical attempt.
    pub(in crate::core) async fn snapshot(&self) -> Vec<Arc<str>> {
        self.registry.alive_snapshot().await
    }

    /// Reports whether a label still owns a physical attempt.
    pub(in crate::core) async fn is_alive(&self, label: &str) -> bool {
        self.registry.is_alive(label).await
    }

    /// Checks registry membership for runtime tests.
    #[cfg(test)]
    pub(in crate::core::runtime) async fn contains_id(&self, id: TaskId) -> bool {
        self.registry.contains(id).await
    }

    /// Exposes available command slots to controller backpressure tests.
    #[cfg(all(test, feature = "controller"))]
    pub(crate) fn registry_command_capacity(&self) -> usize {
        self.cmd_tx.capacity()
    }

    /// Resolves the current registry owner of a label in tests.
    #[cfg(test)]
    pub(crate) async fn id_for_label(&self, label: &str) -> Option<TaskId> {
        self.registry.id_for_label(label).await
    }
}
