//! # Task registry - event-driven task lifecycle manager.
//!
//! Subscribes to Bus and manages active task actors:
//! - On `TaskAddRequested(spec)`  → spawn actor, insert handle, publish `TaskAdded`
//! - On `TaskRemoveRequested(name)` → cancel token, await join, publish `TaskRemoved`
//! - On `ActorExhausted(name)` / `ActorDead(name)` → remove handle, await join, publish `TaskRemoved`
//! - On runtime shutdown (registry listener stops) → `cancel_all()` (cancel → join → `TaskRemoved` for each)
//!
//! ## Architecture
//! ```text
//! Bus → Registry.event_listener()
//!         ├─► TaskAddRequested(spec)    → spawn_and_register(spec) → publish TaskAdded
//!         ├─► TaskRemoveRequested(name) → cancel_and_remove(name)  → publish TaskRemoved
//!         ├─► ActorExhausted(name)      → cleanup_task(name)       → publish TaskRemoved
//!         └─► ActorDead(name)           → cleanup_task(name)       → publish TaskRemoved
//! ```
//!
//! ## Rules
//! - Registry owns JoinHandle + CancellationToken for each actor
//! - Cleanup is event-driven (no polling) and idempotent (safe on duplicates/races)
//! - Does **not** react to `TaskStopped` directly (cleanup happens on actor terminal events)
//! - Publishes `TaskAdded`/`TaskRemoved` for observability
//! - Exposes `wait_became_nonempty_once` / `wait_until_empty` via internal notifiers

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Notify, RwLock, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::core::actor::{ActorExitReason, TaskActor, TaskActorParams};
use crate::events::{Bus, Event, EventKind};
use crate::tasks::TaskSpec;

struct Handle {
    join: JoinHandle<ActorExitReason>,
    cancel: CancellationToken,
}

/// Event-driven registry of active task actors.
pub struct Registry {
    tasks: RwLock<HashMap<String, Handle>>,
    bus: Bus,
    runtime_token: CancellationToken,
    semaphore: Option<Arc<Semaphore>>,
    nonempty_notify: Notify,
    empty_notify: Notify,
}

impl Registry {
    /// Creates a new registry instance.
    pub fn new(
        bus: Bus,
        runtime_token: CancellationToken,
        semaphore: Option<Arc<Semaphore>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            tasks: RwLock::new(HashMap::new()),
            bus,
            runtime_token,
            semaphore,
            nonempty_notify: Notify::new(),
            empty_notify: Notify::new(),
        })
    }

    #[inline]
    fn notify_after_insert(&self, was_empty: bool, len_after: usize) {
        if was_empty && len_after == 1 {
            self.nonempty_notify.notify_waiters();
        }
    }

    #[inline]
    fn notify_after_remove(&self, len_after: usize) {
        if len_after == 0 {
            self.empty_notify.notify_waiters();
        }
    }

    /// One-shot wait until the registry becomes non-empty.
    pub async fn wait_became_nonempty_once(&self) {
        if !self.is_empty().await {
            return;
        }
        self.nonempty_notify.notified().await;
    }

    /// Wait until the registry becomes empty.
    pub async fn wait_until_empty(&self) {
        if self.is_empty().await {
            return;
        }
        self.empty_notify.notified().await;
    }

    /// Spawns the event listener task.
    pub fn spawn_listener(self: Arc<Self>) {
        let mut rx = self.bus.subscribe();
        let rt = self.runtime_token.clone();
        let me = self.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = rt.cancelled() => break,
                    msg = rx.recv() => match msg {
                        Ok(ev) => me.handle_event(&ev).await,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            me.bus.publish(
                                Event::new(EventKind::TaskFailed)
                                    .with_reason("registry_listener_lagged")
                            );
                            continue;
                        }
                    }
                }
            }

            me.cancel_all().await;
        });
    }

    /// Handles incoming events.
    async fn handle_event(&self, event: &Event) {
        match event.kind {
            EventKind::TaskAddRequested => {
                if let Some(spec) = &event.spec {
                    self.spawn_and_register(spec.clone()).await;
                }
            }
            EventKind::TaskRemoveRequested => {
                if let Some(name) = &event.task {
                    self.remove_task(name).await;
                }
            }
            EventKind::ActorExhausted | EventKind::ActorDead => {
                if let Some(name) = &event.task {
                    self.cleanup_task(name).await;
                }
            }
            _ => {}
        }
    }

    /// Returns sorted list of active task names.
    pub async fn list(&self) -> Vec<String> {
        let tasks = self.tasks.read().await;
        let mut names: Vec<String> = tasks.keys().cloned().collect();
        names.sort_unstable();
        names
    }

    /// Returns `true` if registry is empty.
    pub async fn is_empty(&self) -> bool {
        self.tasks.read().await.is_empty()
    }

    /// Cancels all tasks in the registry: cancel → join → TaskRemoved.
    pub async fn cancel_all(&self) {
        let handles: Vec<(String, Handle)> = {
            let mut tasks = self.tasks.write().await;
            let drained = tasks.drain().collect::<Vec<_>>();
            self.empty_notify.notify_waiters();
            drained
        };
        for (_, h) in &handles {
            h.cancel.cancel();
        }
        for (name, h) in handles {
            self.join_and_report(&name, h.join).await;
        }
    }

    /// Spawns an actor and registers its handle.
    async fn spawn_and_register(&self, spec: TaskSpec) {
        let task_name = spec.task().name().to_string();
        {
            let tasks = self.tasks.read().await;
            if tasks.contains_key(&task_name) {
                self.bus.publish(
                    Event::new(EventKind::TaskFailed)
                        .with_task(&task_name)
                        .with_reason("task_already_exists"),
                );
                return;
            }
        }

        let task_token = self.runtime_token.child_token();

        let actor = TaskActor::new(
            self.bus.clone(),
            spec.task().clone(),
            TaskActorParams {
                restart: spec.restart(),
                backoff: spec.backoff(),
                timeout: spec.timeout(),
            },
            self.semaphore.clone(),
        );

        let task_token_clone = task_token.clone();
        let join_handle = tokio::spawn(async move { actor.run(task_token_clone).await });

        let handle = Handle {
            join: join_handle,
            cancel: task_token,
        };

        let mut tasks = self.tasks.write().await;
        let was_empty = tasks.is_empty();
        let inserted = tasks.insert(task_name.clone(), handle).is_none();
        let len_after = tasks.len();
        drop(tasks);

        if inserted {
            self.notify_after_insert(was_empty, len_after);
            self.bus
                .publish(Event::new(EventKind::TaskAdded).with_task(&task_name));
        } else {
            self.bus.publish(
                Event::new(EventKind::TaskFailed)
                    .with_task(&task_name)
                    .with_reason("task_already_exists_race"),
            );
        }
    }

    /// Removes a task and cancels its token.
    async fn remove_task(&self, name: &str) {
        if let Some((handle, len_after)) = self.take_handle_with_len(name).await {
            self.notify_after_remove(len_after);

            handle.cancel.cancel();
            self.join_and_report(name, handle.join).await;
        } else {
            self.bus.publish(
                Event::new(EventKind::TaskFailed)
                    .with_task(name)
                    .with_reason("task_not_found"),
            );
        }
    }

    /// Cleanup finished task (called on ActorExhausted/ActorDead).
    async fn cleanup_task(&self, name: &str) {
        if let Some((handle, len_after)) = self.take_handle_with_len(name).await {
            self.notify_after_remove(len_after);
            self.join_and_report(name, handle.join).await;
        }
    }

    /// Atomically remove handle from registry and return it with the new length.
    async fn take_handle_with_len(&self, name: &str) -> Option<(Handle, usize)> {
        let mut tasks = self.tasks.write().await;
        let h = tasks.remove(name)?;
        let len_after = tasks.len();
        Some((h, len_after))
    }

    /// Await join; report panic as ActorDead(actor_panic); always emit TaskRemoved.
    async fn join_and_report(&self, name: &str, join: JoinHandle<ActorExitReason>) {
        match join.await {
            Ok(_) => {}
            Err(_) => {
                self.bus.publish(
                    Event::new(EventKind::ActorDead)
                        .with_task(name)
                        .with_reason("actor_panic"),
                );
            }
        }
        self.bus
            .publish(Event::new(EventKind::TaskRemoved).with_task(name));
    }
}
