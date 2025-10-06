//! # Task registry - event-driven task lifecycle manager.
//!
//! Registry subscribes to Bus events and manages active tasks:
//! - Listens for `TaskAddRequested` → spawns actor and adds to registry
//! - Listens for `TaskRemoveRequested` → cancels and removes task
//! - Listens for `ActorExhausted` / `ActorDead` → auto-cleanup finished tasks
//!
//! ## Architecture
//! ```text
//! Bus → Registry.event_listener()
//!         ├─► TaskAddRequested(spec) → spawn_and_register(spec)
//!         ├─► TaskRemoveRequested(name) → cancel_and_remove(name)
//!         ├─► ActorExhausted(name) → cleanup_task(name)
//!         └─► ActorDead(name) → cleanup_task(name)
//! ```
//!
//! ## Rules
//! - Registry owns the task handles (JoinHandle + CancellationToken)
//! - Actor spawning happens inside registry (not in supervisor)
//! - Cleanup is automatic via events (no polling needed)
//! - All operations are event-driven (idempotent)

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

pub struct Registry {
    tasks: RwLock<HashMap<String, Handle>>,
    bus: Bus,
    runtime_token: CancellationToken,
    semaphore: Option<Arc<Semaphore>>,
    nonempty_notify: Notify,
    empty_notify: Notify,
}

impl Registry {
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

    pub async fn wait_became_nonempty_once(&self) {
        if !self.is_empty().await {
            return;
        }
        self.nonempty_notify.notified().await;
    }

    pub async fn wait_until_empty(&self) {
        if self.is_empty().await {
            return;
        }
        self.empty_notify.notified().await;
    }

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
                                Event::now(EventKind::TaskFailed)
                                    .with_error("registry_listener_lagged")
                            );
                            continue;
                        }
                    }
                }
            }

            me.cancel_all().await;
        });
    }

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

    pub async fn list(&self) -> Vec<String> {
        let tasks = self.tasks.read().await;
        let mut names: Vec<String> = tasks.keys().cloned().collect();
        names.sort_unstable();
        names
    }

    pub async fn is_empty(&self) -> bool {
        self.tasks.read().await.is_empty()
    }

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

    async fn spawn_and_register(&self, spec: TaskSpec) {
        let task_name = spec.task().name().to_string();

        {
            let tasks = self.tasks.read().await;
            if tasks.contains_key(&task_name) {
                self.bus.publish(
                    Event::now(EventKind::TaskFailed)
                        .with_task(&task_name)
                        .with_error("task_already_exists"),
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
                .publish(Event::now(EventKind::TaskAdded).with_task(&task_name));
        } else {
            self.bus.publish(
                Event::now(EventKind::TaskFailed)
                    .with_task(&task_name)
                    .with_error("task_already_exists_race"),
            );
        }
    }

    async fn remove_task(&self, name: &str) {
        if let Some((handle, len_after)) = self.take_handle_with_len(name).await {
            handle.cancel.cancel();
            self.join_and_report(name, handle.join).await;
            self.notify_after_remove(len_after);
        } else {
            self.bus.publish(
                Event::now(EventKind::TaskFailed)
                    .with_task(name)
                    .with_error("task_not_found"),
            );
        }
    }

    async fn cleanup_task(&self, name: &str) {
        if let Some((handle, len_after)) = self.take_handle_with_len(name).await {
            self.join_and_report(name, handle.join).await;
            self.notify_after_remove(len_after);
        }
    }

    async fn take_handle_with_len(&self, name: &str) -> Option<(Handle, usize)> {
        let mut tasks = self.tasks.write().await;
        let h = tasks.remove(name)?;
        let len_after = tasks.len();
        Some((h, len_after))
    }

    async fn join_and_report(&self, name: &str, join: JoinHandle<ActorExitReason>) {
        match join.await {
            Ok(_) => {}
            Err(_) => {
                self.bus.publish(
                    Event::now(EventKind::ActorDead)
                        .with_task(name)
                        .with_error("actor_panic"),
                );
            }
        }
        self.bus
            .publish(Event::now(EventKind::TaskRemoved).with_task(name));
    }
}