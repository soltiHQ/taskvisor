//! Shared helpers for integration tests.
#![allow(dead_code)]

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use taskvisor::TaskContext;

use taskvisor::{
    BackoffPolicy, Event, EventKind, JitterPolicy, Subscribe, TaskError, TaskFn, TaskId, TaskRef,
};

/// A [`Subscribe`] implementation that records every event it receives.
pub struct EventCollector {
    events: Mutex<Vec<Event>>,
}

impl EventCollector {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
        })
    }

    pub fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }

    pub fn count(&self, kind: EventKind) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.kind == kind)
            .count()
    }

    pub fn by_id(&self, id: TaskId) -> Vec<Event> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.id == Some(id))
            .cloned()
            .collect()
    }

    pub fn by_label(&self, name: &str) -> Vec<Event> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.task.as_deref() == Some(name))
            .cloned()
            .collect()
    }

    pub fn find(&self, kind: EventKind) -> Option<Event> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.kind == kind)
            .cloned()
    }

    pub fn find_all(&self, kind: EventKind) -> Vec<Event> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.kind == kind)
            .cloned()
            .collect()
    }

    pub fn any_reason_contains(&self, kind: EventKind, needle: &str) -> bool {
        self.events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.kind == kind && e.reason.as_deref().is_some_and(|r| r.contains(needle)))
    }
}

impl Subscribe for EventCollector {
    fn on_event(&self, ev: &Event) {
        self.events.lock().unwrap().push(ev.clone());
    }

    fn name(&self) -> &'static str {
        "collector"
    }

    fn queue_capacity(&self) -> usize {
        8192
    }
}

pub async fn poll_until<F, Fut>(within: Duration, mut cond: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let start = std::time::Instant::now();
    loop {
        if cond().await {
            return true;
        }
        if start.elapsed() >= within {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub async fn with_timeout<F: Future>(secs: u64, fut: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(secs), fut)
        .await
        .expect("operation timed out — possible deadlock/hang regression")
}

pub fn make_coop(name: &str) -> TaskRef {
    TaskFn::arc(name, |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    })
}

pub fn make_ok_once(name: &str) -> TaskRef {
    TaskFn::arc(name, |_ctx: TaskContext| async move { Ok(()) })
}

pub fn make_fail(name: &str, exit_code: Option<i32>) -> TaskRef {
    TaskFn::arc(name, move |_ctx: TaskContext| async move {
        Err(TaskError::fail("boom").with_exit_code(exit_code))
    })
}

pub fn make_fatal(name: &str, exit_code: Option<i32>) -> TaskRef {
    TaskFn::arc(name, move |_ctx: TaskContext| async move {
        Err(TaskError::fatal("unrecoverable").with_exit_code(exit_code))
    })
}

pub fn make_panic(name: &str) -> TaskRef {
    TaskFn::arc(name, |_ctx: TaskContext| async move {
        panic!("kaboom");
    })
}

pub fn fast_backoff() -> BackoffPolicy {
    BackoffPolicy::new(
        Duration::from_millis(1),
        Duration::from_millis(1),
        1.0,
        JitterPolicy::None,
    )
    .expect("valid backoff")
}
