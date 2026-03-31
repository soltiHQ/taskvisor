//! # Task registry - event-driven task lifecycle manager.
//!
//! Manages active task actors using two input channels:
//! - **Command channel** (`mpsc`): guaranteed delivery for `Add`/`Remove` commands.
//! - **Event bus** (`broadcast`): lifecycle events from actors (`ActorExhausted`, `ActorDead`).
//!
//! ## Architecture
//! ```text
//! Supervisor ─► bus.publish(TaskAddRequested) ─► Bus (observability, fire-and-forget)
//!            ─► cmd_tx.send(Add(spec)) ────────► Registry.spawn_listener()
//!                                                   ├─► Add(spec)            ─► spawn_and_register
//!                                                   ├─► Remove(name)         ─► cancel_and_remove
//!                                                   ├─► ActorExhausted(name) ─► cleanup_task
//!                                                   └─► ActorDead(name)      ─► cleanup_task
//! ```
//!
//! ## Rules
//!
//! - Does **not** react to `TaskStopped` directly (cleanup happens on actor terminal events)
//! - Cleanup is event-driven *(no polling)* and idempotent (safe on duplicates/races)
//! - Exposes `wait_until_empty` via internal notifier for shutdown coordination
//! - Registry owns JoinHandle + CancellationToken for each actor
//! - Publishes `TaskAdded`/`TaskRemoved` for observability

use std::{collections::HashMap, sync::Arc};

use tokio::sync::{Notify, RwLock, Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::core::actor::{ActorExitReason, TaskActor, TaskActorParams};
use crate::events::{Bus, Event, EventKind};
use crate::tasks::TaskSpec;

/// Command sent via guaranteed-delivery channel (mpsc).
///
/// Separated from the broadcast bus to ensure control commands are never lost to `Lagged`.
/// The Supervisor publishes observability events on the bus before sending the command.
pub(crate) enum RegistryCommand {
    /// Add a new task. Supervisor publishes `TaskAddRequested` on bus before sending.
    Add(TaskSpec),
    /// Remove a task by name. Supervisor publishes `TaskRemoveRequested` on bus before sending.
    Remove(Arc<str>),
}

struct Handle {
    join: JoinHandle<ActorExitReason>,
    cancel: CancellationToken,
}

/// Event-driven registry of active task actors.
///
/// See the [module-level documentation](self) for the dual-channel architecture.
///
/// # Also
///
/// - [`Supervisor`](super::supervisor::Supervisor) - sends commands via mpsc, owns the registry
/// - [`TaskActor`](super::actor::TaskActor) - per-task supervisor spawned by this registry
/// - [`Bus`](crate::events::Bus) - delivers actor terminal events for cleanup
pub(crate) struct Registry {
    tasks: RwLock<HashMap<Arc<str>, Handle>>,
    bus: Bus,
    runtime_token: CancellationToken,
    semaphore: Option<Arc<Semaphore>>,
    empty_notify: Notify,
    cmd_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<RegistryCommand>>>,
}

impl Registry {
    /// Creates a new registry instance.
    pub fn new(
        bus: Bus,
        runtime_token: CancellationToken,
        semaphore: Option<Arc<Semaphore>>,
        cmd_rx: mpsc::UnboundedReceiver<RegistryCommand>,
    ) -> Arc<Self> {
        Arc::new(Self {
            tasks: RwLock::new(HashMap::new()),
            bus,
            runtime_token,
            semaphore,
            empty_notify: Notify::new(),
            cmd_rx: std::sync::Mutex::new(Some(cmd_rx)),
        })
    }

    #[inline]
    fn notify_after_remove(&self, len_after: usize) {
        if len_after == 0 {
            self.empty_notify.notify_one();
        }
    }

    /// Wait until the registry becomes empty.
    ///
    /// Uses the register-before-check pattern to avoid race conditions:
    /// `notified()` is created **before** checking the condition, but `.await`ed **after**.
    pub async fn wait_until_empty(&self) {
        loop {
            let notified = self.empty_notify.notified();
            if self.is_empty().await {
                return;
            }
            notified.await;
        }
    }

    /// Spawns the event listener task.
    ///
    /// Consumes the command receiver stored during construction.
    /// Listens on two channels via `select!`:
    /// - **cmd_rx** (mpsc): guaranteed-delivery commands (`Add`, `Remove`).
    /// - **bus_rx** (broadcast): actor lifecycle events (`ActorExhausted`, `ActorDead`)
    pub fn spawn_listener(self: Arc<Self>) {
        let mut cmd_rx = self
            .cmd_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("spawn_listener called exactly once");

        let mut bus_rx = self.bus.subscribe();
        let rt = self.runtime_token.clone();
        let me = self.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;

                    _ = rt.cancelled() => break,

                    cmd = cmd_rx.recv() => match cmd {
                        Some(RegistryCommand::Add(spec)) => {
                            me.spawn_and_register(spec).await;
                        }
                        Some(RegistryCommand::Remove(name)) => {
                            me.remove_task(&name).await;
                        }
                        None => break,
                    },

                    msg = bus_rx.recv() => match msg {
                        Ok(ev) => me.handle_bus_event(&ev).await,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            me.bus.publish(
                                Event::new(EventKind::SubscriberOverflow)
                                    .with_task("registry")
                                    .with_reason(format!("registry_listener_lagged({})", n))
                            );
                            continue;
                        }
                    }
                }
            }

            me.cancel_all().await;
        });
    }

    /// Handles lifecycle events from the bus.
    ///
    /// Only processes actor terminal events.
    async fn handle_bus_event(&self, event: &Event) {
        match event.kind {
            EventKind::ActorExhausted | EventKind::ActorDead => {
                if let Some(name) = &event.task {
                    self.cleanup_task(name).await;
                }
            }
            _ => {}
        }
    }

    /// Returns sorted list of active task names.
    pub async fn list(&self) -> Vec<Arc<str>> {
        let tasks = self.tasks.read().await;
        let mut names: Vec<Arc<str>> = tasks.keys().cloned().collect();
        names.sort_unstable();
        names
    }

    /// Returns `true` if the registry contains a task with the given name.
    pub async fn contains(&self, name: &str) -> bool {
        self.tasks.read().await.contains_key(name)
    }

    /// Returns `true` if registry is empty.
    pub async fn is_empty(&self) -> bool {
        self.tasks.read().await.is_empty()
    }

    /// Cancels all tasks in the registry: cancel → join → TaskRemoved.
    pub async fn cancel_all(&self) {
        let handles: Vec<(Arc<str>, Handle)> = {
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
        let task_name: Arc<str> = Arc::from(spec.task().name());

        let mut tasks = self.tasks.write().await;
        if tasks.contains_key(&*task_name) {
            drop(tasks);
            self.bus.publish(
                Event::new(EventKind::TaskAdded)
                    .with_task(task_name)
                    .with_reason("already_exists"),
            );
            return;
        }

        let task_token = self.runtime_token.child_token();

        let actor = TaskActor::new(
            self.bus.clone(),
            spec.task().clone(),
            TaskActorParams {
                restart: spec.restart(),
                backoff: spec.backoff(),
                timeout: spec.timeout(),
                max_retries: spec.max_retries(),
            },
            self.semaphore.clone(),
        );

        let task_token_clone = task_token.clone();
        let join_handle = tokio::spawn(async move { actor.run(task_token_clone).await });

        let handle = Handle {
            join: join_handle,
            cancel: task_token,
        };

        tasks.insert(task_name.clone(), handle);
        drop(tasks);

        self.bus
            .publish(Event::new(EventKind::TaskAdded).with_task(task_name));
    }

    /// Removes a task and cancels its token.
    async fn remove_task(&self, name: &str) {
        if let Some((handle, len_after)) = self.take_handle_with_len(name).await {
            self.notify_after_remove(len_after);

            handle.cancel.cancel();
            self.join_and_report(name, handle.join).await;
        } else {
            self.bus.publish(
                Event::new(EventKind::TaskRemoved)
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
