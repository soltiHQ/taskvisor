//! # Tracks currently alive (running) tasks.
//!
//! [`AliveTracker`] subscribes to runtime events and maintains a set of active task names.
//! It listens for [`EventKind::TaskStarting`] and [`EventKind::TaskStopped`] to update its state.
//!
//! This is primarily used by the [`Supervisor`](crate::core::supervisor::Supervisor) to report which tasks
//! are still alive during graceful shutdown.
//!
//! # High-level architecture:
//! ```text
//!  TaskActor ── publish(Event) ──► Bus
//!                                   │
//!                              subscribe()
//!                                   │
//!                                   ▼
//!                  AliveTracker (in-memory set of task names)
//!                         │                  │
//!          TaskStarting ──┘                  └── TaskStopped
//!          insert(name)                          remove(name)
//!
//! Snapshot:
//!   AliveTracker::snapshot() ──► Vec<String> of alive tasks
//!                     └────────► Used by Supervisor during graceful shutdown
//! ```
//!
//! - Actors publish [`EventKind::TaskStarting`] / [`EventKind::TaskStopped`] into the bus.
//! - [`AliveTracker`] listens in background and updates its set accordingly.
//! - [`Supervisor`](crate::core::supervisor::Supervisor) calls `snapshot()` to detect “stuck” tasks after grace timeout.

use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::event::{Event, EventKind};

/// Tracks which tasks are currently alive (running).
///
/// Listens for task lifecycle events via a broadcast channel:
/// - [`EventKind::TaskStarting`] inserts the task name.
/// - [`EventKind::TaskStopped`] removes the task name.
///
/// Used by the [`Supervisor`](crate::core::supervisor::Supervisor) during graceful shutdown to identify "stuck" tasks.
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

    /// Spawns a background listener that subscribes to the given event stream.
    ///
    /// Updates the internal set of alive tasks based on `TaskStarting` and `TaskStopped` events.
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

    /// Returns a snapshot of currently alive tasks as a vector of names.
    pub async fn snapshot(&self) -> Vec<String> {
        let g = self.inner.lock().await;
        g.iter().cloned().collect()
    }
}
