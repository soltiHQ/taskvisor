//! Shared helpers for integration tests.
#![allow(dead_code)]

use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use taskvisor::TaskContext;

use taskvisor::{
    BackoffPolicy, Event, EventKind, JitterPolicy, Subscribe, Supervisor, SupervisorConfig,
    SupervisorHandle, TaskError, TaskFn, TaskId, TaskRef,
};

/// A [`Subscribe`] implementation that records every event it receives.
pub struct EventCollector {
    events: Mutex<Vec<Event>>,
    changes: tokio::sync::watch::Sender<u64>,
}

impl EventCollector {
    pub fn new() -> Arc<Self> {
        let (changes, _initial_receiver) = tokio::sync::watch::channel(0);
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
            changes,
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

    /// Waits for an event-only predicate without timer-based polling.
    ///
    /// The watch version closes the check/notification race: an event published
    /// between the predicate check and `changed()` is retained by the channel.
    pub async fn wait_until<F>(&self, within: Duration, predicate: F) -> bool
    where
        F: Fn(&[Event]) -> bool,
    {
        let deadline = tokio::time::Instant::now() + within;
        let mut changes = self.changes.subscribe();

        loop {
            if predicate(&self.events.lock().unwrap()) {
                return true;
            }
            if tokio::time::timeout_at(deadline, changes.changed())
                .await
                .is_err()
            {
                return predicate(&self.events.lock().unwrap());
            }
        }
    }

    pub async fn wait_for(&self, kind: EventKind, within: Duration) -> Option<Event> {
        self.wait_until(within, |events| {
            events.iter().any(|event| event.kind == kind)
        })
        .await
        .then(|| self.find(kind))
        .flatten()
    }
}

impl Subscribe for EventCollector {
    fn on_event(&self, ev: &Event) {
        self.events.lock().unwrap().push(ev.clone());
        self.changes
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    fn name(&self) -> &str {
        "collector"
    }

    fn queue_capacity(&self) -> NonZeroUsize {
        NonZeroUsize::new(8192).unwrap()
    }
}

pub fn collector_subscribers(collector: &Arc<EventCollector>) -> Vec<Arc<dyn Subscribe>> {
    vec![Arc::clone(collector) as Arc<dyn Subscribe>]
}

pub fn supervisor_with_collector(
    config: SupervisorConfig,
) -> (Arc<Supervisor>, Arc<EventCollector>) {
    let collector = EventCollector::new();
    let supervisor = Supervisor::new(config, collector_subscribers(&collector));
    (supervisor, collector)
}

pub fn served_with_collector(config: SupervisorConfig) -> (SupervisorHandle, Arc<EventCollector>) {
    let (supervisor, collector) = supervisor_with_collector(config);
    (supervisor.serve(), collector)
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

/// A task that starts observably and then ignores cancellation forever.
pub fn make_stubborn(name: &str) -> (TaskRef, Arc<tokio::sync::Notify>) {
    let started = Arc::new(tokio::sync::Notify::new());
    let task_started = Arc::clone(&started);
    let task = TaskFn::arc(name, move |_ctx: TaskContext| {
        let started = Arc::clone(&task_started);
        async move {
            started.notify_one();
            std::future::pending::<()>().await;
            Ok(())
        }
    });
    (task, started)
}

pub async fn wait_for_start(name: &str, started: &tokio::sync::Notify) {
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap_or_else(|_| panic!("{name} did not start"));
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
