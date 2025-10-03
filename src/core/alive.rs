//! # Task lifecycle tracker with sequence-based ordering.
//!
//! Maintains authoritative state of which tasks are currently alive,
//! using event sequence numbers to handle out-of-order delivery.
//!
//! ## Architecture
//! ```text
//! Supervisor ──► Bus ──► subscriber_listener() ──► AliveTracker::update()
//!                                                         │
//!                                                         ▼
//!                                              HashMap<String, TaskState>
//!                                                  (name → {seq, alive})
//! ```
//!
//! ## Rules
//! - Only `TaskStarting` / `TaskStopped` / `TaskFailed` change alive state
//! - Read operations (`snapshot`, `is_alive`) are **eventually consistent**
//! - Other events **update seq** but don't affect alive status
//! - Events with `seq <= last_seq` are **rejected** (stale)

use crate::events::{Event, EventKind};
use std::collections::HashMap;
use tokio::sync::RwLock;

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
/// ### Rules
/// - **Ordering**: events with `seq <= last_seq` are rejected
/// - **State changes**: alive=true or alive=false
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

    /// Updates task state if event is newer than last seen.
    ///
    /// ### Ordering guarantees
    /// Events are applied only if `ev.seq > last_seq` for this task.
    /// This prevents out-of-order events from corrupting state:
    /// ```text
    /// update(TaskStopped, seq=100)  → alive=false, last_seq=100
    /// update(TaskStarting, seq=99)  → rejected (stale)
    /// ```
    ///
    /// ### State transitions
    /// - `TaskStarting` → alive=true, update seq
    /// - `TaskStopped` → alive=false, update seq
    /// - `TaskFailed` → alive=false, update seq
    /// - Other events → no state change, update seq only
    pub async fn update(&self, ev: &Event) -> bool {
        let name = match ev.task.as_deref() {
            Some(n) => n,
            None => return false,
        };

        let mut state = self.state.write().await;
        let entry = state.entry(name.to_string()).or_insert(TaskState {
            last_seq: 0,
            alive: false,
        });

        if ev.seq <= entry.last_seq {
            return false;
        }
        match ev.kind {
            EventKind::TaskStarting => {
                entry.last_seq = ev.seq;
                entry.alive = true;
                true
            }
            EventKind::TaskStopped | EventKind::TaskFailed => {
                entry.last_seq = ev.seq;
                entry.alive = false;
                true
            }
            _ => {
                entry.last_seq = ev.seq;
                false
            }
        }
    }

    /// Returns sorted list of currently alive task names.
    ///
    /// Used by [`Supervisor`](crate::Supervisor) to detect stuck tasks
    /// during graceful shutdown (tasks that didn't stop within grace period).
    pub async fn snapshot(&self) -> Vec<String> {
        let state = self.state.read().await;
        let mut alive: Vec<String> = state
            .iter()
            .filter(|(_, ts)| ts.alive)
            .map(|(name, _)| name.clone())
            .collect();
        alive.sort_unstable();
        alive
    }

    /// Returns true if task is currently alive
    pub async fn is_alive(&self, name: &str) -> bool {
        self.state
            .read()
            .await
            .get(name)
            .map(|ts| ts.alive)
            .unwrap_or(false)
    }
}
