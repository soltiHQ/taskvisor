//! # Task lifecycle tracker with sequence-based ordering.
//!
//! Maintains authoritative state of which tasks are currently alive,
//! using event sequence numbers to handle out-of-order delivery.
//!
//! ## Architecture
//! ```text
//! Supervisor ──► Bus ──► subscriber_listener() ──► AliveTracker::update()
//!                                                         ▼
//!                                              HashMap<String, TaskState>
//!                                                  (name → {seq, alive})
//! ```
//!
//! ## Rules
//! - State toggles only on selected events (see below); all others just advance `last_seq`.
//! - **Alive = true** on `TaskStarting`.
//! - **Alive = false** on `TaskStopped`, `TaskFailed`, `ActorExhausted`, `ActorDead`, `TaskRemoved`.
//! - Events with `seq <= last_seq` for the task are **rejected** (stale).
//! - Read operations (`snapshot`, `is_alive`) are **eventually consistent**.

use std::collections::HashMap;

use tokio::sync::RwLock;

use crate::events::{Event, EventKind};

/// Per-task state for ordering validation.
#[derive(Debug, Clone)]
struct TaskState {
    /// Last seen sequence number for this task.
    last_seq: u64,
    /// Current status (true = alive, false = stopped).
    alive: bool,
}

/// Thread-safe tracker of alive tasks.
///
/// ### Responsibilities
/// - Provides snapshots for graceful shutdown (stuck task detection)
/// - Maintains authoritative state of which tasks are alive
/// - Rejects stale events using sequence numbers
///
/// ### Ordering
/// - Events are applied only if `ev.seq > last_seq` for the task.
pub struct AliveTracker {
    state: RwLock<HashMap<String, TaskState>>,
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
    /// Returns `true` if the **alive flag** changed; returns `false` otherwise,
    /// including when only `last_seq` advanced or the event was ignored as stale.
    ///
    /// ### Ordering guarantees
    /// Events are applied strictly in sequence order — only if `ev.seq > last_seq`
    /// for this task. This prevents out-of-order updates from corrupting state:
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
            return match map.entry(name.to_string()) {
                std::collections::hash_map::Entry::Occupied(entry) => {
                    if ev.seq <= entry.get().last_seq {
                        false
                    } else {
                        entry.remove();
                        true
                    }
                }
                std::collections::hash_map::Entry::Vacant(_) => false,
            };
        }
        let entry = map.entry(name.to_string()).or_insert(TaskState {
            last_seq: 0,
            alive: false,
        });
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
    /// Used by [`Supervisor`](crate::Supervisor) to detect stuck tasks
    /// during graceful shutdown (tasks that didn't stop within grace period).
    pub async fn snapshot(&self) -> Vec<String> {
        let state = self.state.read().await;
        let alive: Vec<String> = state
            .iter()
            .filter(|(_, ts)| ts.alive)
            .map(|(name, _)| name.clone())
            .collect();
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
