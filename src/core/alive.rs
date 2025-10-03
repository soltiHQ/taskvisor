use crate::{Event, EventKind};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
struct TaskState {
    /// Last seen sequence number for this task
    last_seq: u64,
    /// Current status (true = alive, false = stopped)
    alive: bool,
}

/// Thread-safe alive tracker with sequence-based ordering
pub struct AliveTracker {
    state: Arc<RwLock<HashMap<String, TaskState>>>,
}

impl AliveTracker {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Updates task state only if event is newer than last seen.
    /// Returns true if state was updated, false if event was stale.
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

        // Reject stale events (seq <= last_seq)
        if ev.seq <= entry.last_seq {
            return false;
        }

        // Update based on event kind
        match ev.kind {
            EventKind::TaskStarting => {
                entry.last_seq = ev.seq;
                entry.alive = true;
                true
            }
            EventKind::TaskStopped => {
                entry.last_seq = ev.seq;
                entry.alive = false;
                true
            }
            _ => {
                // Other events don't change alive state, but update seq
                entry.last_seq = ev.seq;
                false
            }
        }
    }

    /// Returns snapshot of currently alive tasks
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
