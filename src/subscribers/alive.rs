//! # Stateful subscriber that tracks currently running tasks.
//!
//! [`AliveTracker`] maintains an in-memory set of active task names by listening to
//! [`EventKind::TaskStarting`] and [`EventKind::TaskStopped`] events.
//!
//! This is used internally by the [`Supervisor`](crate::core::Supervisor) during
//! graceful shutdown to identify tasks that haven't stopped within the grace period.
//!
//! ## Architecture
//! ```text
//!  TaskActor ── publish(Event) ──► Bus
//!                                   │
//!                              subscribe()
//!                                   │
//!                                   ▼
//!                  AliveTracker (HashSet<String> behind Mutex)
//!                         │                  │
//!          TaskStarting ──┘                  └── TaskStopped
//!          insert(name)                          remove(name)
//!
//! During shutdown:
//!   Supervisor calls AliveTracker::snapshot() ──► Vec<String> of stuck tasks
//! ```
//!
//! ## Example
//! ```no_run
//! # use taskvisor::subscribers::AliveTracker;
//! # use taskvisor::events::Bus;
//! # async fn demo() {
//! let bus = Bus::new(1024);
//! let tracker = AliveTracker::new();
//!
//! // Spawn background listener
//! tracker.spawn_listener(bus.subscribe());
//!
//! // Later, get snapshot of running tasks
//! let running_tasks = tracker.snapshot().await;
//! println!("Currently running: {:?}", running_tasks);
//! # }
//! ```

use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::events::{Event, EventKind};

/// Tracks which tasks are currently alive (running).
///
/// Maintains an internal HashSet of task names, updated based on lifecycle events.
/// Thread-safe and cloneable - multiple references share the same internal state.
#[derive(Clone)]
pub struct AliveTracker {
    inner: Arc<Mutex<HashSet<String>>>,
}

impl AliveTracker {
    /// Creates a new, empty tracker.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Spawns a background task that subscribes to events and updates the tracker.
    ///
    /// The spawned task will:
    /// - Insert task names on `TaskStarting` events
    /// - Remove task names on `TaskStopped` events
    /// - Exit when the receiver is dropped or the bus is destroyed
    ///
    /// # Parameters
    /// - `rx`: Broadcast receiver subscribed to the event bus
    pub fn spawn_listener(&self, mut rx: tokio::sync::broadcast::Receiver<Event>) {
        let inner = self.inner.clone();

        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                match ev.kind {
                    EventKind::TaskStarting => {
                        if let Some(name) = ev.task.clone() {
                            inner.lock().await.insert(name);
                        }
                    }
                    EventKind::TaskStopped => {
                        if let Some(name) = ev.task.clone() {
                            inner.lock().await.remove(&name);
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    /// Returns a snapshot of currently alive task names.
    ///
    /// # Returns
    /// Vector of task names that have started but not yet stopped.
    pub async fn snapshot(&self) -> Vec<String> {
        let g = self.inner.lock().await;
        g.iter().cloned().collect()
    }
}

impl Default for AliveTracker {
    fn default() -> Self {
        Self::new()
    }
}
