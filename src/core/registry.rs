//! # Task registry - event-driven task lifecycle manager.
//!
//! Manages active task actors using two input channels:
//! - **Command channel** (`mpsc`): guaranteed delivery for `Add`/`Remove` commands.
//! - **Event bus** (`broadcast`): lifecycle events from actors (`ActorExhausted`, `ActorDead`).
//!
//! ## Architecture
//! ```text
//! Supervisor ─► bus.publish(TaskAddRequested) ─► Bus (observability, fire-and-forget)
//!            ─► cmd_tx.send(Add(id, spec)) ────► Registry.spawn_listener()
//!                                                   ├─► Add(id, spec)      ─► spawn_and_register
//!                                                   ├─► Remove(id)         ─► cancel_and_remove
//!                                                   ├─► ActorExhausted(id) ─► cleanup_task
//!                                                   └─► ActorDead(id)      ─► cleanup_task
//!
//! Tasks are keyed by the runtime [`TaskId`] (identity); the task name is a human label.
//! ```
//!
//! ## Rules
//!
//! - Does **not** react to `TaskStopped` directly (cleanup happens on actor terminal events)
//! - Cleanup is event-driven *(no polling)* and idempotent (safe on duplicates/races)
//! - Exposes `wait_until_empty` via internal notifier for shutdown coordination
//! - Registry owns JoinHandle + CancellationToken for each actor
//! - Publishes `TaskAdded`/`TaskRemoved` for observability

use std::{collections::HashMap, sync::Arc, time::Duration};

use tokio::sync::{Notify, RwLock, Semaphore, mpsc, oneshot};
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::core::actor::{ActorExitReason, TaskActor, TaskActorParams};
use crate::core::outcome::TaskOutcome;
use crate::events::{Bus, Event, EventKind};
use crate::identity::TaskId;
use crate::tasks::TaskSpec;

/// One-shot sender resolving a [`TaskWaiter`](crate::TaskWaiter) with the final outcome.
pub(crate) type OutcomeTx = oneshot::Sender<TaskOutcome>;

/// Command sent via guaranteed-delivery channel (mpsc).
pub(crate) enum RegistryCommand {
    /// Add a new task with its runtime identity and an optional outcome watcher.
    Add(TaskId, TaskSpec, Option<OutcomeTx>),
    /// Remove a task by its runtime identity. Supervisor publishes `TaskRemoveRequested` first.
    Remove(TaskId),
}

struct Handle {
    join: JoinHandle<ActorExitReason>,
    cancel: CancellationToken,
    label: Arc<str>,
    done: Option<OutcomeTx>,
}

/// Registry maps held under a single lock so identity and label stay consistent.
#[derive(Default)]
struct Inner {
    /// Identity → handle (the canonical map; the [`TaskId`] is the key).
    tasks: HashMap<TaskId, Handle>,
    /// Label → identity (optional dedup gate + label-addressed cancel/remove).
    by_label: HashMap<Arc<str>, TaskId>,
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
    state: RwLock<Inner>,
    bus: Bus,
    runtime_token: CancellationToken,
    semaphore: Option<Arc<Semaphore>>,
    grace: Duration,
    empty_notify: Notify,
    cmd_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<RegistryCommand>>>,
    pending_joins: Arc<std::sync::Mutex<HashMap<TaskId, usize>>>,
}

impl Registry {
    /// Creates a new registry instance.
    pub fn new(
        bus: Bus,
        runtime_token: CancellationToken,
        semaphore: Option<Arc<Semaphore>>,
        grace: Duration,
        cmd_rx: mpsc::UnboundedReceiver<RegistryCommand>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: RwLock::new(Inner::default()),
            bus,
            runtime_token,
            semaphore,
            grace,
            empty_notify: Notify::new(),
            cmd_rx: std::sync::Mutex::new(Some(cmd_rx)),
            pending_joins: Arc::new(std::sync::Mutex::new(HashMap::new())),
        })
    }

    /// Increments the pending-join refcount for `id`.
    fn pending_inc(pending: &std::sync::Mutex<HashMap<TaskId, usize>>, id: TaskId) {
        let mut map = pending.lock().unwrap_or_else(|e| e.into_inner());
        *map.entry(id).or_insert(0) += 1;
    }

    /// Decrements the pending-join refcount for `id`, dropping the entry at zero.
    fn pending_dec(pending: &std::sync::Mutex<HashMap<TaskId, usize>>, id: TaskId) {
        let mut map = pending.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = map.get_mut(&id) {
            *n -= 1;
            if *n == 0 {
                map.remove(&id);
            }
        }
    }

    /// Returns `true` once the task is fully terminated: not registered and not awaiting join.
    ///
    /// Used as a state fallback when a `TaskRemoved` event may have been lost to broadcast lag.
    pub async fn is_terminated(&self, id: TaskId) -> bool {
        if self.state.read().await.tasks.contains_key(&id) {
            return false;
        }
        !self
            .pending_joins
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&id)
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
                        Some(RegistryCommand::Add(id, spec, done)) => {
                            me.spawn_and_register(id, spec, done).await;
                        }
                        Some(RegistryCommand::Remove(id)) => {
                            me.remove_task(id).await;
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
                            me.reap_finished().await;
                            continue;
                        }
                    }
                }
            }
            me.cancel_all_within(Duration::ZERO).await;
        });
    }

    /// Handles lifecycle events from the bus.
    ///
    /// Only processes actor terminal events.
    async fn handle_bus_event(&self, event: &Event) {
        match event.kind {
            EventKind::ActorExhausted | EventKind::ActorDead => {
                if let Some(id) = event.id {
                    self.cleanup_task(id).await;
                }
            }
            _ => {}
        }
    }

    /// Returns active tasks as `(id, label)` pairs, sorted by identity.
    pub async fn list(&self) -> Vec<(TaskId, Arc<str>)> {
        let st = self.state.read().await;
        let mut v: Vec<(TaskId, Arc<str>)> = st
            .tasks
            .iter()
            .map(|(id, h)| (*id, h.label.clone()))
            .collect();
        v.sort_by_key(|(id, _)| *id);
        v
    }

    /// Returns `true` if a task with the given identity is registered.
    pub async fn contains(&self, id: TaskId) -> bool {
        self.state.read().await.tasks.contains_key(&id)
    }

    /// Resolves a label to the identity currently holding it (if any).
    pub async fn id_for_label(&self, name: &str) -> Option<TaskId> {
        self.state.read().await.by_label.get(name).copied()
    }

    /// Returns `true` if registry is empty.
    pub async fn is_empty(&self) -> bool {
        self.state.read().await.tasks.is_empty()
    }

    /// Cancels all tasks, waits up to `grace` for cooperative exit, then force-aborts stragglers.
    ///
    /// Steps:
    /// - drain the registry and cancel every task token,
    /// - join each actor, bounded by a single shared `grace` deadline,
    /// - any actor still running when the deadline passes is force-terminated via [`JoinHandle::abort`] and reported in the returned "stuck" set.
    ///
    /// Always publishes `TaskRemoved` for every task (and `ActorDead` on panic).
    pub async fn cancel_all_within(&self, grace: Duration) -> Vec<Arc<str>> {
        let grace = grace.min(Duration::from_secs(60 * 60 * 24 * 365 * 30));
        let handles: Vec<(TaskId, Handle)> = {
            let mut st = self.state.write().await;
            st.by_label.clear();
            let drained = st.tasks.drain().collect::<Vec<_>>();
            self.empty_notify.notify_waiters();
            drained
        };
        for (id, h) in &handles {
            Self::pending_inc(&self.pending_joins, *id);
            h.cancel.cancel();
        }

        let deadline = tokio::time::Instant::now() + grace;
        let mut stuck = Vec::new();

        for (id, h) in handles {
            let label = h.label.clone();
            let mut join = h.join;
            match tokio::time::timeout_at(deadline, &mut join).await {
                Ok(res) => {
                    Self::pending_dec(&self.pending_joins, id);
                    Self::report_join(&self.bus, id, &label, res, h.done);
                }
                Err(_elapsed) => {
                    join.abort();
                    let _ = join.await;
                    Self::pending_dec(&self.pending_joins, id);
                    if let Some(done) = h.done {
                        let _ = done.send(TaskOutcome::ForceAborted);
                    }
                    self.bus.publish(
                        Event::new(EventKind::TaskRemoved)
                            .with_task(Arc::clone(&label))
                            .with_id(id)
                            .with_reason("force_terminated_after_grace"),
                    );
                    stuck.push(label);
                }
            }
        }
        stuck
    }

    /// Spawns an actor and registers its handle under the given runtime identity.
    ///
    /// On a duplicate label the optional `done` sender is resolved with [`TaskOutcome::Rejected`](crate::TaskOutcome) (reason `already_exists`).
    /// Plain `add_and_watch` callers still don't observe it: they see `TaskAddFailed` first and discard the waiter before it is handed out.
    async fn spawn_and_register(&self, id: TaskId, spec: TaskSpec, done: Option<OutcomeTx>) {
        let label: Arc<str> = Arc::from(spec.task().name());

        let mut st = self.state.write().await;
        if st.by_label.contains_key(&label) {
            drop(st);
            if let Some(done) = done {
                let _ = done.send(TaskOutcome::Rejected {
                    reason: Arc::from("already_exists"),
                });
            }
            self.bus.publish(
                Event::new(EventKind::TaskAddFailed)
                    .with_task(label)
                    .with_id(id)
                    .with_reason("already_exists"),
            );
            return;
        }

        let task_token = self.runtime_token.child_token();

        let actor = TaskActor::new(
            self.bus.clone(),
            label.clone(),
            spec.task().clone(),
            TaskActorParams {
                restart: spec.restart(),
                backoff: spec.backoff(),
                timeout: spec.timeout(),
                max_retries: spec.max_retries(),
            },
            self.semaphore.clone(),
            id,
        );

        let task_token_clone = task_token.clone();
        let join_handle = tokio::spawn(async move { actor.run(task_token_clone).await });

        st.tasks.insert(
            id,
            Handle {
                join: join_handle,
                cancel: task_token,
                label: label.clone(),
                done,
            },
        );
        st.by_label.insert(label.clone(), id);
        drop(st);

        self.bus.publish(
            Event::new(EventKind::TaskAdded)
                .with_task(label)
                .with_id(id),
        );
    }

    /// Removes a task by identity.
    async fn remove_task(&self, id: TaskId) {
        Self::pending_inc(&self.pending_joins, id);
        if let Some((handle, len_after)) = self.take_handle(id).await {
            self.notify_after_remove(len_after);

            handle.cancel.cancel();
            self.spawn_join_report(id, handle.label, handle.join, Some(self.grace), handle.done);
        } else {
            Self::pending_dec(&self.pending_joins, id);
            self.bus.publish(
                Event::new(EventKind::TaskRemoved)
                    .with_id(id)
                    .with_reason("task_not_found"),
            );
        }
    }

    /// Cleanup finished task by identity.
    async fn cleanup_task(&self, id: TaskId) {
        Self::pending_inc(&self.pending_joins, id);
        if let Some((handle, len_after)) = self.take_handle(id).await {
            self.notify_after_remove(len_after);
            self.spawn_join_report(id, handle.label, handle.join, None, handle.done);
        } else {
            Self::pending_dec(&self.pending_joins, id);
        }
    }

    /// Atomically remove a handle by identity (and its label-index entry), returning it with the new task count.
    async fn take_handle(&self, id: TaskId) -> Option<(Handle, usize)> {
        let mut st = self.state.write().await;
        let h = st.tasks.remove(&id)?;
        st.by_label.remove(&h.label);
        let len_after = st.tasks.len();
        Some((h, len_after))
    }

    /// Joins the actor handle in a detached task and reports its terminal events.
    fn spawn_join_report(
        &self,
        id: TaskId,
        name: Arc<str>,
        join: JoinHandle<ActorExitReason>,
        force_after: Option<Duration>,
        done: Option<OutcomeTx>,
    ) {
        let bus = self.bus.clone();
        let pending = Arc::clone(&self.pending_joins);
        tokio::spawn(async move {
            let mut join = join;
            match force_after {
                Some(grace) => match tokio::time::timeout(grace, &mut join).await {
                    Ok(res) => {
                        Self::pending_dec(&pending, id);
                        Self::report_join(&bus, id, &name, res, done);
                    }
                    Err(_) => {
                        join.abort();
                        let _ = join.await;
                        Self::pending_dec(&pending, id);
                        if let Some(done) = done {
                            let _ = done.send(TaskOutcome::ForceAborted);
                        }
                        bus.publish(
                            Event::new(EventKind::TaskRemoved)
                                .with_task(name)
                                .with_id(id)
                                .with_reason("force_terminated_after_grace"),
                        );
                    }
                },
                None => {
                    let res = join.await;
                    Self::pending_dec(&pending, id);
                    Self::report_join(&bus, id, &name, res, done);
                }
            }
        });
    }

    /// Removes and reports any actors whose task has already finished.
    async fn reap_finished(&self) {
        let finished: Vec<TaskId> = {
            let st = self.state.read().await;
            st.tasks
                .iter()
                .filter(|(_, h)| h.join.is_finished())
                .map(|(id, _)| *id)
                .collect()
        };
        for id in finished {
            self.cleanup_task(id).await;
        }
    }

    /// Publish terminal events for a joined actor: `ActorDead` if it panicked, then always `TaskRemoved`.
    fn report_join(
        bus: &Bus,
        id: TaskId,
        name: &str,
        res: Result<ActorExitReason, JoinError>,
        done: Option<OutcomeTx>,
    ) {
        if let Err(e) = &res
            && e.is_panic()
        {
            bus.publish(
                Event::new(EventKind::ActorDead)
                    .with_task(name)
                    .with_id(id)
                    .with_reason("actor_panic"),
            );
        }
        if let Some(done) = done {
            let _ = done.send(Self::outcome_of(res));
        }
        bus.publish(
            Event::new(EventKind::TaskRemoved)
                .with_task(name)
                .with_id(id),
        );
    }

    /// Maps a joined actor result to the public [`TaskOutcome`].
    fn outcome_of(res: Result<ActorExitReason, JoinError>) -> TaskOutcome {
        match res {
            Ok(ActorExitReason::Completed) => TaskOutcome::Completed,
            Ok(ActorExitReason::Canceled) => TaskOutcome::Canceled,
            Ok(ActorExitReason::Exhausted {
                reason,
                exit_code,
                source,
            }) => TaskOutcome::Failed {
                reason,
                exit_code,
                source,
            },
            Ok(ActorExitReason::Fatal {
                reason,
                exit_code,
                source,
            }) => TaskOutcome::Fatal {
                reason,
                exit_code,
                source,
            },
            Err(e) if e.is_panic() => TaskOutcome::Panicked,
            Err(_aborted) => TaskOutcome::ForceAborted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Arc<Registry> {
        let bus = Bus::new(64);
        let token = CancellationToken::new();
        let (_tx, rx) = mpsc::unbounded_channel();
        Registry::new(bus, token, None, Duration::from_secs(5), rx)
    }

    #[tokio::test]
    async fn reap_finished_removes_completed_handles() {
        let reg = registry();

        let join = tokio::spawn(async { ActorExitReason::Completed });
        while !join.is_finished() {
            tokio::task::yield_now().await;
        }
        reg.state.write().await.tasks.insert(
            TaskId::next(),
            Handle {
                join,
                cancel: CancellationToken::new(),
                label: Arc::from("done"),
                done: None,
            },
        );
        assert!(!reg.is_empty().await);

        reg.reap_finished().await;
        assert!(
            reg.is_empty().await,
            "reap_finished must drop the completed handle"
        );
    }

    #[tokio::test]
    async fn reap_finished_keeps_running_handles() {
        let reg = registry();

        let cancel = CancellationToken::new();
        let child = cancel.clone();
        let join = tokio::spawn(async move {
            child.cancelled().await;
            ActorExitReason::Canceled
        });
        reg.state.write().await.tasks.insert(
            TaskId::next(),
            Handle {
                join,
                cancel,
                label: Arc::from("running"),
                done: None,
            },
        );

        reg.reap_finished().await;
        assert!(!reg.is_empty().await, "a running actor must not be reaped");
    }
}
