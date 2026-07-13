//! # Best-effort alive-task tracker.
//!
//! Tracks which task runs are currently marked alive based on lifecycle events.
//!
//! This is a read-side cache used for snapshots and shutdown diagnostics.
//! The registry remains the authoritative owner of task membership.
//!
//! ## Flow
//!
//! ```text
//! runtime bus -> subscriber listener -> AliveTracker::update() -> HashMap<AliveKey, TaskState>
//! ```
//!
//! ## Keys
//!
//! Entries are keyed by [`TaskId`] when an event has one.
//! For older/id-less events, the task name is used as a fallback key.
//!
//! Task names are labels and may be reused across runs.
//! A late event from an old run with a different id cannot change the new run's state.
//!
//! ## Rules
//!
//! - Other events are ignored and do not create entries or advance `last_seq`.
//! - Events with `seq <= last_seq` for the same key are ignored as stale.
//! - A fresh `TaskRemoved` removes the entry entirely.
//! - Only lifecycle events are tracked.
//! - `TaskStopped`, `TaskCanceled`, `TaskFailed`, `ActorExhausted`, and`ActorDead` mark a task as not alive.
//! - `snapshot` and `is_alive` are eventually consistent because they are based on the lossy event bus.
//! - `reconcile` prunes id-keyed entries that are no longer present in the registry live-id set.
//! - `TaskStarting` marks a task as alive.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::RwLock;

use crate::events::{Event, EventKind};
use crate::identity::TaskId;

/// Tracker key: task id when available, task name as fallback.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum AliveKey {
    Id(TaskId),
    Name(Arc<str>),
}

/// Cached alive state for one tracker key.
#[derive(Debug, Clone)]
struct TaskState {
    /// Task name reported by `is_alive` and `snapshot`.
    name: Arc<str>,
    /// Last applied event sequence for this key.
    last_seq: u64,
    /// Whether this task run is currently marked alive.
    alive: bool,
}

/// Thread-safe best-effort tracker of alive tasks.
///
/// Used by runtime snapshots and shutdown diagnostics.
/// The tracker applies only lifecycle events and rejects stale events per key.
pub(crate) struct AliveTracker {
    state: RwLock<HashMap<AliveKey, TaskState>>,
}

impl AliveTracker {
    /// Creates a new empty tracker.
    pub fn new() -> Self {
        Self {
            state: RwLock::new(HashMap::new()),
        }
    }

    /// Applies a lifecycle event to the tracked state.
    ///
    /// Only task lifecycle events are tracked.
    /// Other events are ignored and do not create entries or advance `last_seq`.
    ///
    /// A fresh [`EventKind::TaskRemoved`] removes the entry entirely.
    /// Stale `TaskRemoved` events are ignored like other stale events.
    ///
    /// ### Stale events
    ///
    /// Events are applied only when `ev.seq > last_seq` for the same identity:
    ///
    /// ```text
    /// update(TaskStopped,  seq=100) -> alive=false, last_seq=100
    /// update(TaskStarting, seq=99)  -> ignored as stale
    /// ```
    pub async fn update(&self, ev: &Event) -> bool {
        let relevant = matches!(
            ev.kind,
            EventKind::TaskStarting
                | EventKind::TaskStopped
                | EventKind::TaskCanceled
                | EventKind::TaskFailed
                | EventKind::ActorExhausted
                | EventKind::ActorDead
                | EventKind::TaskRemoved
        );
        if !relevant {
            return false;
        }

        let key = match (ev.id, ev.task.as_deref()) {
            (Some(id), _) => AliveKey::Id(id),
            (None, Some(name)) => AliveKey::Name(Arc::from(name)),
            (None, None) => return false,
        };

        let mut map = self.state.write().await;

        if matches!(ev.kind, EventKind::TaskRemoved) {
            return match map.get(&key) {
                Some(state) if ev.seq > state.last_seq => {
                    map.remove(&key);
                    true
                }
                _ => false,
            };
        }

        let Some(name) = ev.task.as_deref() else {
            return false;
        };

        let entry = map.entry(key).or_insert_with(|| TaskState {
            name: Arc::from(name),
            last_seq: 0,
            alive: false,
        });

        if ev.seq <= entry.last_seq {
            return false;
        }

        let next_alive = match ev.kind {
            EventKind::TaskStarting => true,
            EventKind::TaskStopped
            | EventKind::TaskCanceled
            | EventKind::TaskFailed
            | EventKind::ActorExhausted
            | EventKind::ActorDead => false,
            _ => entry.alive,
        };

        let changed = next_alive != entry.alive;
        entry.alive = next_alive;
        entry.last_seq = ev.seq;
        changed
    }

    /// Prunes id-keyed entries that are no longer present in the registry live set.
    pub async fn reconcile(&self, live_ids: &HashSet<TaskId>) {
        let mut map = self.state.write().await;
        map.retain(|key, _| match key {
            AliveKey::Id(id) => live_ids.contains(id),
            AliveKey::Name(_) => true,
        });
    }

    /// Returns a sorted, deduplicated list of task names currently marked alive.
    pub async fn snapshot(&self) -> Vec<Arc<str>> {
        let state = self.state.read().await;
        let mut alive: Vec<Arc<str>> = state
            .values()
            .filter(|ts| ts.alive)
            .map(|ts| ts.name.clone())
            .collect();
        alive.sort_unstable();
        alive.dedup();
        alive
    }

    /// Returns true if any task run with this name is currently marked alive.
    pub async fn is_alive(&self, name: &str) -> bool {
        self.state
            .read()
            .await
            .values()
            .any(|ts| ts.alive && &*ts.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: EventKind, task: &str, seq: u64) -> Event {
        let mut e = Event::new(kind).with_task(task);
        e.seq = seq;
        e
    }

    fn evi(kind: EventKind, task: &str, seq: u64, id: crate::identity::TaskId) -> Event {
        let mut e = Event::new(kind).with_task(task).with_id(id);
        e.seq = seq;
        e
    }

    #[tokio::test]
    async fn non_lifecycle_events_do_not_pollute_entries() {
        let tracker = AliveTracker::new();
        let id = crate::identity::TaskId::next();

        tracker
            .update(&evi(EventKind::BackoffScheduled, "slot-name", 1, id))
            .await;
        tracker
            .update(&evi(EventKind::TaskStarting, "real-name", 2, id))
            .await;

        assert!(
            tracker.is_alive("real-name").await,
            "lifecycle event must bind the id to the lifecycle task name"
        );
        assert!(
            !tracker.is_alive("slot-name").await,
            "non-lifecycle event must not have created an entry"
        );
    }

    #[tokio::test]
    async fn late_removed_from_old_id_keeps_new_task_alive() {
        let tracker = AliveTracker::new();
        let old = crate::identity::TaskId::next();
        let new = crate::identity::TaskId::next();

        tracker
            .update(&evi(EventKind::TaskStarting, "x", 1, old))
            .await;
        tracker
            .update(&evi(EventKind::TaskStarting, "x", 3, new))
            .await;
        tracker
            .update(&evi(EventKind::TaskRemoved, "x", 4, old))
            .await;

        assert!(
            tracker.is_alive("x").await,
            "late TaskRemoved of the previous run must not kill the new run's state"
        );
        assert!(
            tracker.snapshot().await.iter().any(|n| &**n == "x"),
            "snapshot must still report the reused label as alive"
        );

        tracker
            .update(&evi(EventKind::TaskRemoved, "x", 5, new))
            .await;
        assert!(!tracker.is_alive("x").await);
    }

    #[tokio::test]
    async fn stale_and_equal_sequence_events_are_rejected() {
        for (incoming_seq, case) in [(99, "stale"), (100, "equal")] {
            let tracker = AliveTracker::new();
            tracker.update(&ev(EventKind::TaskStopped, "t1", 100)).await;

            let changed = tracker
                .update(&ev(EventKind::TaskStarting, "t1", incoming_seq))
                .await;
            assert!(!changed, "{case} event must not change state");
            assert!(
                !tracker.is_alive("t1").await,
                "task must remain dead after a {case} event"
            );
        }
    }

    #[tokio::test]
    async fn task_removed_deletes_entry() {
        let tracker = AliveTracker::new();
        assert!(
            tracker.update(&ev(EventKind::TaskStarting, "t1", 1)).await,
            "first TaskStarting should change alive from false to true"
        );
        assert!(tracker.is_alive("t1").await);

        let changed = tracker.update(&ev(EventKind::TaskRemoved, "t1", 2)).await;
        assert!(changed, "TaskRemoved should report change");
        assert!(!tracker.is_alive("t1").await);

        tracker.update(&ev(EventKind::TaskStarting, "t1", 3)).await;
        assert!(tracker.is_alive("t1").await, "fresh entry after removal");
    }

    #[tokio::test]
    async fn stale_task_removed_ignored() {
        let tracker = AliveTracker::new();
        tracker.update(&ev(EventKind::TaskStarting, "t1", 10)).await;

        let changed = tracker.update(&ev(EventKind::TaskRemoved, "t1", 5)).await;
        assert!(!changed);
        assert!(tracker.is_alive("t1").await, "task must remain alive");
    }

    #[tokio::test]
    async fn task_removed_for_unknown_task_is_a_noop() {
        let tracker = AliveTracker::new();
        let changed = tracker
            .update(&ev(EventKind::TaskRemoved, "ghost", 1))
            .await;
        assert!(!changed, "removing unknown task is a no-op");
        assert!(!tracker.is_alive("ghost").await);
        assert!(tracker.snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn snapshot_returns_alive_sorted() {
        let tracker = AliveTracker::new();
        tracker
            .update(&ev(EventKind::TaskStarting, "charlie", 1))
            .await;
        tracker
            .update(&ev(EventKind::TaskStarting, "alpha", 2))
            .await;
        tracker
            .update(&ev(EventKind::TaskStarting, "bravo", 3))
            .await;
        tracker
            .update(&ev(EventKind::TaskStopped, "bravo", 4))
            .await;

        let alive = tracker.snapshot().await;
        let names: Vec<&str> = alive.iter().map(|a| &**a).collect();
        assert_eq!(names, vec!["alpha", "charlie"]);
    }

    #[tokio::test]
    async fn event_without_task_name_ignored() {
        let tracker = AliveTracker::new();
        let mut e = Event::new(EventKind::TaskStarting);
        e.seq = 1;
        let changed = tracker.update(&e).await;
        assert!(!changed);
        assert!(tracker.snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn non_lifecycle_event_is_ignored_and_keeps_alive() {
        let tracker = AliveTracker::new();
        tracker.update(&ev(EventKind::TaskStarting, "t1", 1)).await;

        let changed = tracker
            .update(&ev(EventKind::BackoffScheduled, "t1", 2))
            .await;
        assert!(!changed, "non-lifecycle event should be ignored");
        assert!(tracker.is_alive("t1").await);

        let changed = tracker.update(&ev(EventKind::TaskStopped, "t1", 2)).await;
        assert!(changed, "TaskStopped with seq=2 should still apply");
        assert!(!tracker.is_alive("t1").await);
    }

    #[tokio::test]
    async fn all_death_events_set_alive_false() {
        for kind in [
            EventKind::TaskStopped,
            EventKind::TaskCanceled,
            EventKind::TaskFailed,
            EventKind::ActorExhausted,
            EventKind::ActorDead,
        ] {
            let tracker = AliveTracker::new();
            tracker.update(&ev(EventKind::TaskStarting, "t", 1)).await;
            let changed = tracker.update(&ev(kind, "t", 2)).await;
            assert!(changed, "{kind:?} should set alive=false");
            assert!(
                !tracker.is_alive("t").await,
                "{kind:?} should make task dead"
            );
        }
    }

    #[tokio::test]
    async fn reconcile_prunes_entries_absent_from_the_live_set() {
        let tracker = AliveTracker::new();
        let kept = crate::identity::TaskId::next();
        let orphan = crate::identity::TaskId::next();

        // Both look alive; `orphan` is the task whose `TaskRemoved` was lost to lag.
        tracker
            .update(&evi(EventKind::TaskStarting, "kept", 1, kept))
            .await;
        tracker
            .update(&evi(EventKind::TaskStarting, "orphan", 2, orphan))
            .await;
        assert!(tracker.is_alive("orphan").await);

        // Registry's authoritative live set no longer contains `orphan`.
        let live: HashSet<TaskId> = [kept].into_iter().collect();
        tracker.reconcile(&live).await;

        assert!(
            tracker.is_alive("kept").await,
            "a task still in the live set must be retained"
        );
        assert!(
            !tracker.is_alive("orphan").await,
            "an entry absent from the live set (lost TaskRemoved) must be pruned"
        );
        assert_eq!(
            tracker.snapshot().await,
            vec![Arc::<str>::from("kept")],
            "snapshot must reflect the reconciled state"
        );
    }
}
