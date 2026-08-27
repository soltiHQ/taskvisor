//! Exposes registry-backed task state to runtime callers without the command queue.
//!
//! [`SupervisorHandle`](crate::SupervisorHandle) uses these reads for membership and current-attempt queries.
//! Membership comes from the registry.
//! Physical activity combines registry activity bits with force-aborted attempts that remain active after membership ends.

use std::sync::Arc;

use super::super::SupervisorCore;
use crate::identity::TaskId;

impl SupervisorCore {
    /// Registry members and entries still completing removal.
    ///
    /// Results are `(id, name)` pairs sorted by identity.
    pub(in crate::core) async fn list_tasks(&self) -> Vec<(TaskId, Arc<str>)> {
        self.registry.list().await
    }

    /// Sorted names that still own a physical attempt.
    pub(in crate::core) async fn snapshot(&self) -> Vec<Arc<str>> {
        self.registry.alive_snapshot().await
    }

    /// Whether a name still owns a physical attempt.
    pub(in crate::core) async fn is_alive(&self, name: &str) -> bool {
        self.registry.is_alive(name).await
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

    /// Resolves the current registry owner of a name in tests.
    #[cfg(test)]
    pub(crate) async fn id_for_name(&self, name: &str) -> Option<TaskId> {
        self.registry.id_for_name(name).await
    }
}
