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

use tokio::sync::{RwLock, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::core::actor::{ActorExitReason, TaskActor, TaskActorParams};
use crate::events::{Bus, Event, EventKind};
use crate::tasks::TaskSpec;

/// Handle to a running task actor.
struct Handle {
    /// Original task specification.
    spec: TaskSpec,
    /// Join handle for the actor's execution.
    join: JoinHandle<ActorExitReason>,
    /// Individual cancellation token for this task.
    cancel: CancellationToken,
}

impl Handle {
    #[allow(dead_code)]
    fn name(&self) -> &str {
        self.spec.task().name()
    }
}

/// Event-driven registry of active tasks.
pub struct Registry {
    tasks: RwLock<HashMap<String, Handle>>,
    bus: Bus,
    runtime_token: CancellationToken,
    semaphore: Option<Arc<Semaphore>>,
}

impl Registry {
    /// Creates a new registry.
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
        })
    }

    /// Spawns event listener that manages task lifecycle.
    ///
    /// Call once during Supervisor init.
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

    /// Returns true if registry is empty.
    pub async fn is_empty(&self) -> bool {
        self.tasks.read().await.is_empty()
    }

    /// Cancels all tasks in the registry: cancel → join → TaskRemoved.
    pub async fn cancel_all(&self) {
        let handles: Vec<(String, Handle)> = {
            let mut tasks = self.tasks.write().await;
            tasks.drain().collect()
        };

        for (_, h) in &handles {
            h.cancel.cancel();
        }

        for (name, h) in handles {
            self.join_and_report(&name, h.join).await;
        }
    }

    /// Spawns actor and adds to registry.
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
            spec,
            join: join_handle,
            cancel: task_token,
        };

        let mut tasks = self.tasks.write().await;
        if tasks.insert(task_name.clone(), handle).is_none() {
            drop(tasks);
            self.bus
                .publish(Event::now(EventKind::TaskAdded).with_task(&task_name));
        } else {
            drop(tasks);
            self.bus.publish(
                Event::now(EventKind::TaskFailed)
                    .with_task(&task_name)
                    .with_error("task_already_exists_race"),
            );
        }
    }

    /// Removes task and cancels its token.
    async fn remove_task(&self, name: &str) {
        if let Some(handle) = self.take_handle(name).await {
            handle.cancel.cancel();
            self.join_and_report(name, handle.join).await;
        } else {
            self.bus.publish(
                Event::now(EventKind::TaskFailed)
                    .with_task(name)
                    .with_error("task_not_found"),
            );
        }
    }

    /// Cleanup finished task (called on ActorExhausted/ActorDead).
    async fn cleanup_task(&self, name: &str) {
        if let Some(handle) = self.take_handle(name).await {
            // актёр уже должен быть завершён; ждём join и репортим
            self.join_and_report(name, handle.join).await;
        }
    }

    // ---------------------------
    // Helpers (DRY)
    // ---------------------------

    /// Atomically remove handle from registry.
    async fn take_handle(&self, name: &str) -> Option<Handle> {
        let mut tasks = self.tasks.write().await;
        tasks.remove(name)
    }

    /// Await join, report panic as ActorDead(actor_panic), always emit TaskRemoved.
    async fn join_and_report(&self, name: &str, join: JoinHandle<ActorExitReason>) {
        match join.await {
            Ok(_reason) => {}
            Err(_je) => {
                self.publish_actor_panic(name, "actor_panic").await;
            }
        }
        self.bus
            .publish(Event::now(EventKind::TaskRemoved).with_task(name));
    }

    async fn publish_actor_panic(&self, name: &str, code: &str) {
        self.bus.publish(
            Event::now(EventKind::ActorDead)
                .with_task(name)
                .with_error(code),
        );
    }
}
