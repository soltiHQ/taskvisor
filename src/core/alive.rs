//! # Task lifecycle tracker with sequence-based ordering.
//!
//! Maintains authoritative state of which tasks are currently alive, using event
//! sequence numbers to handle out-of-order delivery.
//!
//! ## Architecture
//!
//! ```text
//! Supervisor ──► Bus ──► subscriber_listener() ──► AliveTracker::update()
//!                                                         ▼
//!                                              HashMap<Arc<str>, TaskState>
//!                                                  (name → {seq, alive})
//! ```
//!
//! ## Rules
//!
//! - State toggles only on selected events (see below); all others just advance `last_seq`.
//! - **Entry removed** on `TaskRemoved` (not just set to false — fully cleaned up).
//! - Read operations (`snapshot`, `is_alive`) are **eventually consistent**.
//! - Events with `seq <= last_seq` for the task are **rejected** (stale).
//! - **Alive = false** on `TaskStopped`, `TaskFailed`, `ActorExhausted`, `ActorDead`.
//! - **Alive = true** on `TaskStarting`.

use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;

use crate::events::{Event, EventKind};

/// Per-task state for ordering validation.
#[derive(Debug, Clone)]
struct TaskState {
    /// Last seen sequence number for this task.
    last_seq: u64,
    /// Current status *(true = alive, false = stopped)*.
    alive: bool,
}

/// Thread-safe tracker of alive tasks.
///
/// ### Responsibilities
///
/// - Provides snapshots for graceful shutdown (stuck task detection)
/// - Maintains authoritative state of which tasks are alive
/// - Rejects stale events using sequence numbers
///
/// ### Ordering
///
/// - Events are applied only if `ev.seq > last_seq` for the task.
///
/// # Also
///
/// - [`Event`](crate::Event) - event payload with `seq` used for ordering
/// - [`EventKind`](crate::EventKind) - classification that drives alive/dead transitions
pub(crate) struct AliveTracker {
    state: RwLock<HashMap<Arc<str>, TaskState>>,
}

impl AliveTracker {
    /// Creates a new empty tracker.
    pub fn new() -> Self {
        Self {
            state: RwLock::new(HashMap::new()),
        }
    }

    /// Updates the tracked state for a task if the incoming event is newer than the last one observed.
    ///
    /// Removes the task entry entirely when receiving [`EventKind::TaskRemoved`].
    /// Returns `true` if the **alive flag** changed; returns `false` otherwise.
    ///
    /// ### Ordering guarantees
    ///
    /// Events are applied strictly in sequence order:  only if `ev.seq > last_seq` for this task.
    /// This prevents out-of-order updates from corrupting state:
    /// ```text
    /// update(TaskStopped,  seq=100) → alive=false, last_seq=100
    /// update(TaskStarting, seq=99)  → ignored (stale)
    /// ```
    pub async fn update(&self, ev: &Event) -> bool {
        let Some(name) = ev.task.as_deref() else {
            return false;
        };
        let mut map = self.state.write().await;

        if matches!(ev.kind, EventKind::TaskRemoved) {
            return match map.get(name) {
                Some(state) if ev.seq > state.last_seq => {
                    map.remove(name);
                    true
                }
                _ => false,
            };
        }
        let entry = if let Some(existing) = map.get_mut(name) {
            existing
        } else {
            map.entry(Arc::from(name)).or_insert(TaskState {
                last_seq: 0,
                alive: false,
            })
        };
        if ev.seq <= entry.last_seq {
            return false;
        }

        let next_alive = match ev.kind {
            EventKind::TaskStarting => true,
            EventKind::TaskStopped
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

    /// Returns a sorted list of currently alive task names.
    ///
    /// Used by [`Supervisor`](crate::Supervisor) to detect stuck tasks during graceful shutdown.
    /// *(tasks that didn't stop within grace period)*.
    pub async fn snapshot(&self) -> Vec<Arc<str>> {
        let state = self.state.read().await;
        let mut alive: Vec<Arc<str>> = state
            .iter()
            .filter(|(_, ts)| ts.alive)
            .map(|(name, _)| name.clone())
            .collect();
        alive.sort_unstable();
        alive
    }

    /// Returns true if the task is currently marked alive.
    pub async fn is_alive(&self, name: &str) -> bool {
        self.state
            .read()
            .await
            .get(name)
            .map(|ts| ts.alive)
            .unwrap_or(false)
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

    #[tokio::test]
    async fn task_starting_sets_alive() {
        let tracker = AliveTracker::new();

        let changed = tracker.update(&ev(EventKind::TaskStarting, "t1", 1)).await;
        assert!(
            changed,
            "first TaskStarting should change alive from false to true"
        );
        assert!(tracker.is_alive("t1").await);
    }

    #[tokio::test]
    async fn task_stopped_sets_dead() {
        let tracker = AliveTracker::new();
        tracker.update(&ev(EventKind::TaskStarting, "t1", 1)).await;

        let changed = tracker.update(&ev(EventKind::TaskStopped, "t1", 2)).await;
        assert!(changed);
        assert!(!tracker.is_alive("t1").await);
    }

    #[tokio::test]
    async fn stale_event_rejected() {
        let tracker = AliveTracker::new();
        tracker.update(&ev(EventKind::TaskStopped, "t1", 100)).await;

        let changed = tracker.update(&ev(EventKind::TaskStarting, "t1", 99)).await;
        assert!(!changed, "stale event must not change state");
        assert!(
            !tracker.is_alive("t1").await,
            "task must remain dead after stale event"
        );
    }

    #[tokio::test]
    async fn equal_seq_rejected() {
        let tracker = AliveTracker::new();
        tracker.update(&ev(EventKind::TaskStopped, "t1", 50)).await;

        let changed = tracker.update(&ev(EventKind::TaskStarting, "t1", 50)).await;
        assert!(!changed);
        assert!(!tracker.is_alive("t1").await);
    }

    #[tokio::test]
    async fn task_removed_deletes_entry() {
        let tracker = AliveTracker::new();
        tracker.update(&ev(EventKind::TaskStarting, "t1", 1)).await;
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
    async fn task_removed_for_unknown_task() {
        let tracker = AliveTracker::new();
        let changed = tracker
            .update(&ev(EventKind::TaskRemoved, "ghost", 1))
            .await;
        assert!(!changed, "removing unknown task is a no-op");
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
    async fn is_alive_unknown_returns_false() {
        let tracker = AliveTracker::new();
        assert!(!tracker.is_alive("nonexistent").await);
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
    async fn non_lifecycle_event_advances_seq_but_keeps_alive() {
        let tracker = AliveTracker::new();
        tracker.update(&ev(EventKind::TaskStarting, "t1", 1)).await;

        let changed = tracker
            .update(&ev(EventKind::BackoffScheduled, "t1", 2))
            .await;
        assert!(!changed, "non-lifecycle event should not change alive flag");
        assert!(tracker.is_alive("t1").await);

        let changed = tracker.update(&ev(EventKind::TaskStopped, "t1", 1)).await;
        assert!(!changed, "seq=1 is stale after seq=2");
        assert!(tracker.is_alive("t1").await);
    }

    #[tokio::test]
    async fn all_death_events_set_alive_false() {
        for kind in [
            EventKind::TaskStopped,
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
}
