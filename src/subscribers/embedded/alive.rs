//! # AliveTracker – track currently running tasks
//!
//! Maintains an in-memory set of **alive** task names by listening to [`EventKind::TaskStarting`] and [`EventKind::TaskStopped`].
//!
//! ## Why?
//! The supervisor can check which tasks are still running during graceful shutdown, and UIs/metrics can display the live set.
//!
//! ## Behavior
//! - Duplicate **start** → tolerated, warn (idempotent insert).
//! - **Stop** without prior start → tolerated, warn.
//!
//! ## Internal scheme
//! ```text
//! on_event(ev):
//!   ├─ if ev.kind == TaskStarting && ev.task => insert(name)
//!   ├─ if ev.kind == TaskStopped  && ev.task => remove(name)
//!   └─ otherwise: ignore
//!
//! snapshot() -> Vec<String>  (sorted copy of the current set)
//! ```

use crate::events::{Event, EventKind};
use crate::subscribers::Subscribe;
use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::RwLock;

/// Tracks the set of currently running task names.
pub struct AliveTracker {
    inner: RwLock<HashSet<String>>,
    capacity: usize,
}

impl AliveTracker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashSet::new()),
            capacity: 2048,
        }
    }

    /// Configure the queue capacity for this subscriber.
    #[must_use]
    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity.max(1);
        self
    }

    /// Returns a snapshot (sorted) of currently alive task names.
    ///
    /// This is a synchronous method using read lock.
    #[must_use]
    pub fn snapshot(&self) -> Vec<String> {
        let g = self.inner.read().unwrap();
        let mut v: Vec<String> = g.iter().cloned().collect();
        v.sort_unstable();
        v
    }

    fn handle_start(set: &mut HashSet<String>, name: &str) {
        if !set.insert(name.to_owned()) {
            eprintln!("[taskvisor] AliveTracker: duplicate start for '{}'", name);
        }
    }

    fn handle_stop(set: &mut HashSet<String>, name: &str) {
        if !set.remove(name) {
            eprintln!(
                "[taskvisor] AliveTracker: stop without start for '{}'",
                name
            );
        }
    }
}

#[async_trait]
impl Subscribe for AliveTracker {
    async fn on_event(&self, ev: &Event) {
        match ev.kind {
            EventKind::TaskStarting => {
                if let Some(name) = ev.task.as_deref() {
                    let mut g = self.inner.write().unwrap();
                    Self::handle_start(&mut g, name);
                }
            }
            EventKind::TaskStopped => {
                if let Some(name) = ev.task.as_deref() {
                    let mut g = self.inner.write().unwrap();
                    Self::handle_stop(&mut g, name);
                }
            }
            _ => {}
        }
    }

    fn name(&self) -> &'static str {
        "AliveTracker"
    }
    fn queue_capacity(&self) -> usize {
        self.capacity
    }
}

impl Default for AliveTracker {
    fn default() -> Self {
        Self::new()
    }
}
