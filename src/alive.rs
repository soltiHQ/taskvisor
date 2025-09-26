use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::event::{Event, EventKind};

#[derive(Clone)]
pub struct AliveTracker {
    inner: Arc<Mutex<HashSet<String>>>,
}

impl AliveTracker {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashSet::new())),
        }
    }

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

    pub async fn snapshot(&self) -> Vec<String> {
        let g = self.inner.lock().await;
        g.iter().cloned().collect()
    }
}
