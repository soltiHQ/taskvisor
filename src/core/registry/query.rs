//! Reads registry membership and physical attempt activity.
//!
//! [`SupervisorHandle`](crate::SupervisorHandle) uses these reads for task lists and activity checks.
//! Calls reach this module through [`SupervisorCore`](crate::core::runtime::SupervisorCore).
//! Static run also waits here for membership to become empty. Queries read shared state directly.
//! They do not pass through the command listener and do not drive lifecycle work.
//!
//! Membership includes registered and removing entries. A force-aborted attempt can remain active
//! after the registry becomes empty. Activity queries combine both sources because they answer
//! whether a task is physically in an attempt.

use std::sync::{Arc, atomic::Ordering};

use super::Registry;
use crate::identity::TaskId;

impl Registry {
    /// Waits until no registered or removing entries remain.
    ///
    /// Notification is registered before the check to prevent a lost wakeup.
    pub async fn wait_until_empty(&self) {
        loop {
            let notified = self.empty_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_empty().await {
                return;
            }
            notified.await;
        }
    }

    /// Returns registered and removing tasks as `(identity, name)` pairs.
    ///
    /// Results are sorted by identity.
    pub async fn list(&self) -> Vec<(TaskId, Arc<str>)> {
        let st = self.state.read().await;
        let mut tasks: Vec<(TaskId, Arc<str>)> = st
            .tasks
            .iter()
            .map(|(id, entry)| (*id, Arc::clone(&entry.name)))
            .collect();
        drop(st);
        tasks.sort_by_key(|(id, _)| *id);
        tasks
    }

    /// Returns whether a name still owns a physical task attempt.
    ///
    /// This checks registry activity and force-aborted attempts that remain active after removal.
    pub(in crate::core) async fn is_alive(&self, name: &str) -> bool {
        let state = self.state.read().await;
        let registered = state.by_name.get(name).is_some_and(|id| {
            state
                .tasks
                .get(id)
                .is_some_and(|entry| entry.activity.load(Ordering::Acquire))
        });
        drop(state);
        registered || self.actors.attempt_reaper().is_alive(name)
    }

    /// Returns sorted names that still own a physical task attempt.
    ///
    /// This combines registry activity with force-aborted attempts that remain active after removal.
    pub(in crate::core) async fn alive_snapshot(&self) -> Vec<Arc<str>> {
        let state = self.state.read().await;
        let mut alive: Vec<_> = state
            .tasks
            .values()
            .filter(|entry| entry.activity.load(Ordering::Acquire))
            .map(|entry| Arc::clone(&entry.name))
            .collect();
        drop(state);
        alive.extend(self.actors.attempt_reaper().alive_names());
        alive.sort_unstable();
        alive.dedup();
        alive
    }

    /// Returns true if `id` is registered or removing.
    #[cfg(test)]
    pub async fn contains(&self, id: TaskId) -> bool {
        self.state.read().await.tasks.contains_key(&id)
    }

    /// Resolves a name to its membership identity, including a removing entry.
    #[cfg(test)]
    pub async fn id_for_name(&self, name: &str) -> Option<TaskId> {
        self.state.read().await.by_name.get(name).copied()
    }

    /// Returns true if no tasks are registered or removing.
    pub async fn is_empty(&self) -> bool {
        self.state.read().await.tasks.is_empty()
    }
}
