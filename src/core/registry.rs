//! # Task registry.
//!
//! Owns registered task actors and their runtime identity.
//!
//! The registry is the authoritative owner of active task membership.
//! It keeps:
//! - `TaskId -> actor handle`,
//! - task name -> `TaskId`,
//! - detached join bookkeeping for shutdown and removal.
//!
//! ## Input Planes
//!
//! ```text
//! Management plane:
//!   SupervisorCore                 Registry
//!        |                            |
//!        |-- command (bounded mpsc) ->|
//!        |                            |-- update registry state
//!        |<-- reply (oneshot) --------|
//!
//! Completion plane:
//!   TaskActor -> completion mpsc -> registry cleanup
//!
//! Observability plane:
//!   TaskActor -> broadcast bus -> subscribers / controller
//! ```
//!
//! ## Flow
//!
//! ```text
//! Add(id, spec)
//!   -> spawn actor
//!   -> insert id/name indexes
//!   -> reply registered
//!   -> publish TaskAdded
//!
//! Remove(id)
//!   -> remove handle from registry
//!   -> cancel actor token
//!   -> reply claimed
//!   -> join actor
//!   -> publish TaskRemoved
//!
//! Actor completion(id)
//!   -> remove handle from registry
//!   -> join actor
//!   -> publish TaskRemoved
//! ```
//!
//! ## Rules
//!
//! - Watched tasks resolve their `TaskOutcome` when the actor join is reported.
//! - Command replies report registry decisions, not terminal task outcomes.
//! - Cleanup is idempotent. Duplicate or stale completion signals become no-ops.
//! - The registry does not clean up on `TaskStopped` or `TaskFailed`.
//! - Task name is a human label and a duplicate-name admission gate.
//!   Cleanup waits for the actor completion signal and join result.
//! - `TaskRemoved` means the registry has finished cleanup for that identity.
//! - `TaskId` is the canonical identity.

use std::{collections::HashMap, future::Future, sync::Arc, time::Duration};

use tokio::sync::{Notify, RwLock, Semaphore, mpsc, oneshot};
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::core::actor::{ActorExitReason, TaskActor, TaskActorParams};
use crate::core::outcome::TaskOutcome;
use crate::error::RuntimeError;
use crate::events::{Bus, Event, EventKind};
use crate::identity::TaskId;
use crate::reasons;
use crate::tasks::TaskSpec;

/// Sender used to resolve a watched task with its final [`TaskOutcome`].
pub(crate) type OutcomeTx = oneshot::Sender<TaskOutcome>;

/// Authoritative result of one registry add command.
pub(crate) type AddReply = Result<(), RuntimeError>;

/// Receiver for an authoritative registry add result.
pub(crate) type AddReplyRx = oneshot::Receiver<AddReply>;

/// Authoritative result of one registry remove command.
///
/// `Ok(true)` means the registry claimed the task and sent cancellation.
/// It does not mean the actor has terminated yet.
pub(crate) type RemoveReply = Result<bool, RuntimeError>;

/// Receiver for an authoritative registry remove result.
pub(crate) type RemoveReplyRx = oneshot::Receiver<RemoveReply>;

/// Command sent to the registry over the management channel.
pub(crate) enum RegistryCommand {
    /// Register a task under a pre-minted runtime identity.
    Add {
        id: TaskId,
        spec: TaskSpec,
        outcome: Option<OutcomeTx>,
        reply: oneshot::Sender<AddReply>,
    },
    /// Remove a task by runtime identity.
    ///
    /// The public caller publishes `TaskRemoveRequested` before sending this.
    Remove {
        id: TaskId,
        reply: oneshot::Sender<RemoveReply>,
    },
}

/// Registry-owned actor handle for one registered task.
struct Handle {
    join: JoinHandle<ActorExitReason>,
    cancel: CancellationToken,
    label: Arc<str>,
    done: Option<OutcomeTx>,
}

/// Sends one completion signal when an actor task returns, panics, or is aborted.
struct ActorCompletionGuard {
    id: TaskId,
    tx: mpsc::UnboundedSender<TaskId>,
}

impl Drop for ActorCompletionGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(self.id);
    }
}

/// Registry indexes guarded by one lock.
///
/// Keeping both maps under the same lock keeps identity and label lookup in sync.
#[derive(Default)]
struct Inner {
    /// Canonical task map keyed by runtime identity.
    tasks: HashMap<TaskId, Handle>,

    /// Label lookup used for duplicate-name checks and label-based operations.
    by_label: HashMap<Arc<str>, TaskId>,
}

/// Mutable state for detached join tracking.
#[derive(Default)]
struct PendingInner {
    /// Number of in-flight join reporters per task identity.
    counts: HashMap<TaskId, usize>,

    /// Human labels used for shutdown diagnostics when joins do not finish in time.
    labels: HashMap<TaskId, Arc<str>>,
}

/// Tracks actor joins that are running outside the registry map.
///
/// This is used after remove/cleanup paths move a handle out of `state.tasks` but still need to wait for the actor join and final `TaskRemoved`.
#[derive(Default)]
struct PendingJoins {
    inner: std::sync::Mutex<PendingInner>,
    drained: Notify,
}

impl PendingJoins {
    /// Marks one join reporter for `id` as in flight.
    fn inc(&self, id: TaskId) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *g.counts.entry(id).or_insert(0) += 1;
    }

    /// Stores the label for an in-flight join.
    ///
    /// No-op if `id` is not currently tracked.
    fn label(&self, id: TaskId, label: Arc<str>) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.counts.contains_key(&id) {
            g.labels.insert(id, label);
        }
    }

    /// Marks one in-flight join for `id` as finished.
    ///
    /// Wakes waiters when no joins remain.
    fn dec(&self, id: TaskId) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = g.counts.get_mut(&id) {
            *n -= 1;
            if *n == 0 {
                g.counts.remove(&id);
                g.labels.remove(&id);
            }
        }
        if g.counts.is_empty() {
            self.drained.notify_waiters();
        }
    }

    /// Returns `true` if a join for `id` is still in flight.
    fn contains(&self, id: TaskId) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .counts
            .contains_key(&id)
    }

    /// Returns `true` if no joins are in flight.
    fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .counts
            .is_empty()
    }

    /// Returns labels for joins still in flight.
    ///
    /// Best-effort: an id that was incremented but not labeled yet is omitted.
    fn pending_labels(&self) -> Vec<Arc<str>> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .labels
            .values()
            .cloned()
            .collect()
    }

    /// Waits until no joins are in flight.
    ///
    /// Uses register-before-check: `notified()` is created before checking `is_empty`; a concurrent `dec` cannot lose the wakeup.
    async fn wait_drained(&self) {
        loop {
            let notified = self.drained.notified();
            if self.is_empty() {
                return;
            }
            notified.await;
        }
    }
}

/// Owns registered task actors and task membership.
///
/// The registry accepts add/remove commands, receives reliable actor completion
/// signals, joins actors after removal or completion, and publishes registry-level
/// lifecycle events such as `TaskAdded` and `TaskRemoved`.
///
/// # Also
///
/// - [`TaskActor`](super::actor::TaskActor) - per-task actor spawned by the registry
/// - [`SupervisorCore`](super::runtime::SupervisorCore) - sends registry commands
/// - [`TaskOutcome`] - final result for watched tasks
pub(crate) struct Registry {
    state: RwLock<Inner>,
    bus: Bus,
    runtime_token: CancellationToken,
    semaphore: Option<Arc<Semaphore>>,
    grace: Duration,
    empty_notify: Notify,
    cmd_rx: std::sync::Mutex<Option<mpsc::Receiver<RegistryCommand>>>,
    completion_tx: mpsc::UnboundedSender<TaskId>,
    completion_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<TaskId>>>,
    pending_joins: Arc<PendingJoins>,
    listener_handle: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl Registry {
    /// Creates a registry with its command receiver and runtime dependencies.
    pub fn new(
        bus: Bus,
        runtime_token: CancellationToken,
        semaphore: Option<Arc<Semaphore>>,
        grace: Duration,
        cmd_rx: mpsc::Receiver<RegistryCommand>,
    ) -> Arc<Self> {
        let (completion_tx, completion_rx) = mpsc::unbounded_channel();
        Arc::new(Self {
            state: RwLock::new(Inner::default()),
            bus,
            runtime_token,
            semaphore,
            grace,
            empty_notify: Notify::new(),
            cmd_rx: std::sync::Mutex::new(Some(cmd_rx)),
            completion_tx,
            completion_rx: std::sync::Mutex::new(Some(completion_rx)),
            pending_joins: Arc::new(PendingJoins::default()),
            listener_handle: std::sync::Mutex::new(None),
        })
    }

    /// Returns true when `id` is no longer registered and has no join in flight.
    ///
    /// Used as a fallback when a `TaskRemoved` event may have been missed because of broadcast lag.
    pub async fn is_terminated(&self, id: TaskId) -> bool {
        if self.state.read().await.tasks.contains_key(&id) {
            return false;
        }
        !self.pending_joins.contains(id)
    }

    /// Waits for detached join reporters to finish.
    ///
    /// Detached join reporters are created after remove/cleanup paths take a task out of `state.tasks`.
    /// They are no longer covered by [`cancel_all_within`](Self::cancel_all_within), but shutdown still needs their final `TaskRemoved` events before the subscriber listener stops.
    ///
    /// Returns labels for joins still in flight after `grace`.
    pub async fn wait_joins_within(&self, grace: Duration) -> Vec<Arc<str>> {
        let _ = tokio::time::timeout(grace, self.pending_joins.wait_drained()).await;
        self.pending_joins.pending_labels()
    }

    #[inline]
    fn notify_after_remove(&self, len_after: usize) {
        if len_after == 0 {
            self.empty_notify.notify_one();
        }
    }

    /// Waits until no tasks remain registered.
    ///
    /// This only checks the registry map.
    /// Detached joins may still be in flight; use [`wait_joins_within`](Self::wait_joins_within) for those.
    ///
    /// Uses register-before-check to avoid losing a wakeup.
    pub async fn wait_until_empty(&self) {
        loop {
            let notified = self.empty_notify.notified();
            if self.is_empty().await {
                return;
            }
            notified.await;
        }
    }

    /// Starts the registry listener task.
    ///
    /// The listener consumes receivers stored during construction.
    /// It listens to:
    /// - management commands from `cmd_rx`,
    /// - actor identity signals from the reliable completion channel.
    ///
    /// On runtime shutdown, it closes the command receiver, drains already buffered commands, cancels remaining actors with zero extra grace, and waits for all join reporters to finish.
    pub fn spawn_listener(self: Arc<Self>) {
        let mut cmd_rx = self
            .cmd_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("spawn_listener called exactly once");
        let mut completion_rx = self
            .completion_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("spawn_listener called exactly once");

        let rt = self.runtime_token.clone();
        let me = self.clone();

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;

                    _ = rt.cancelled() => break,

                    completed = completion_rx.recv() => match completed {
                        Some(id) => me.guarded("registry", me.cleanup_task(id)).await,
                        None => break,
                    },

                    cmd = cmd_rx.recv() => match cmd {
                        Some(RegistryCommand::Add { id, spec, outcome, reply }) => {
                            me.guarded("registry", me.spawn_and_register(id, spec, outcome, reply))
                            .await;
                        }
                        Some(RegistryCommand::Remove { id, reply }) => {
                            me.guarded("registry", me.remove_task(id, reply)).await;
                        }
                        None => break,
                    }
                }
            }

            cmd_rx.close();
            completion_rx.close();
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    RegistryCommand::Add {
                        id,
                        spec,
                        outcome,
                        reply,
                    } => {
                        me.guarded("registry", me.spawn_and_register(id, spec, outcome, reply))
                            .await;
                    }
                    RegistryCommand::Remove { id, reply } => {
                        me.guarded("registry", me.remove_task(id, reply)).await;
                    }
                }
            }
            me.cancel_all_within(Duration::ZERO).await;
            me.pending_joins.wait_drained().await;
        });

        *self
            .listener_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    /// Waits for the registry listener task to finish.
    ///
    /// Safe to call after shutdown has started.
    /// If the listener was never started, this is a no-op.
    pub async fn join_listener(&self) {
        let handle = self
            .listener_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    /// Runs one listener operation under a panic boundary.
    ///
    /// A panic while processing one command/event is reported as a diagnostic event instead of killing the whole registry listener.
    async fn guarded(&self, who: &'static str, fut: impl Future<Output = ()>) {
        if let Err(msg) = crate::core::panic_guard::guarded(fut).await {
            self.bus.publish(Event::subscriber_panicked(
                who,
                format!("listener panic: {msg}"),
            ));
        }
    }

    /// Returns registered tasks as `(id, label)` pairs, sorted by identity.
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

    /// Returns true if `id` is currently registered.
    pub async fn contains(&self, id: TaskId) -> bool {
        self.state.read().await.tasks.contains_key(&id)
    }

    /// Resolves a label to the identity currently holding it (if any).
    pub async fn id_for_label(&self, name: &str) -> Option<TaskId> {
        self.state.read().await.by_label.get(name).copied()
    }

    /// Returns true if no tasks are currently registered.
    ///
    /// Detached joins may still be running after the map becomes empty.
    pub async fn is_empty(&self) -> bool {
        self.state.read().await.tasks.is_empty()
    }

    /// Cancels all registered tasks and waits for them within one shared grace window.
    ///
    /// Steps:
    /// - remove all handles from the registry map,
    /// - cancel every actor token,
    /// - join each actor until the shared deadline,
    /// - abort actors that do not finish in time,
    /// - publish `TaskRemoved` for each drained task.
    ///
    /// Returns labels of tasks that were force-aborted.
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
            self.pending_joins.inc(*id);
            h.cancel.cancel();
        }

        let deadline = tokio::time::Instant::now() + grace;
        let mut stuck = Vec::new();

        for (id, h) in handles {
            let label = h.label.clone();
            let mut join = h.join;
            match tokio::time::timeout_at(deadline, &mut join).await {
                Ok(res) => {
                    self.pending_joins.dec(id);
                    Self::report_join(&self.bus, id, &label, res, h.done);
                }
                Err(_elapsed) => {
                    join.abort();
                    let _ = join.await;
                    self.pending_joins.dec(id);
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
        let _ = tokio::time::timeout_at(deadline, self.pending_joins.wait_drained()).await;
        stuck
    }

    /// Spawns an actor future with a reliable identity completion signal.
    fn spawn_tracked_actor(
        id: TaskId,
        completion_tx: mpsc::UnboundedSender<TaskId>,
        future: impl Future<Output = ActorExitReason> + Send + 'static,
    ) -> JoinHandle<ActorExitReason> {
        let completion = ActorCompletionGuard {
            id,
            tx: completion_tx,
        };
        tokio::spawn(async move {
            let _completion = completion;
            future.await
        })
    }

    /// Spawns an actor and registers it under `id`.
    ///
    /// Duplicate task names are rejected.
    /// If a watched add includes `done`, it resolves as [`TaskOutcome::Rejected`]
    /// with reason [`ALREADY_EXISTS`](crate::reasons::ALREADY_EXISTS).
    ///
    /// Direct `add_and_watch` callers still receive [`RuntimeError::TaskAlreadyExists`](crate::RuntimeError::TaskAlreadyExists) because registration confirmation fails before the waiter is returned.
    async fn spawn_and_register(
        &self,
        id: TaskId,
        spec: TaskSpec,
        done: Option<OutcomeTx>,
        reply: oneshot::Sender<AddReply>,
    ) {
        let label: Arc<str> = Arc::from(spec.task().name());

        let mut st = self.state.write().await;
        if st.by_label.contains_key(&label) {
            drop(st);
            let _ = reply.send(Err(RuntimeError::TaskAlreadyExists {
                name: Arc::clone(&label),
            }));
            if let Some(done) = done {
                let _ = done.send(TaskOutcome::Rejected {
                    reason: Arc::from(reasons::ALREADY_EXISTS),
                });
            }
            self.bus.publish(
                Event::new(EventKind::TaskAddFailed)
                    .with_task(label)
                    .with_id(id)
                    .with_reason(reasons::ALREADY_EXISTS),
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
        let join_handle =
            Self::spawn_tracked_actor(id, self.completion_tx.clone(), actor.run(task_token_clone));

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

        let _ = reply.send(Ok(()));
        self.bus.publish(
            Event::new(EventKind::TaskAdded)
                .with_task(label)
                .with_id(id),
        );
    }

    /// Removes a task by identity.
    ///
    /// If the task exists, its actor token is cancelled and a detached join reporter publishes the final `TaskRemoved`.
    ///
    /// If the task is unknown or cleanup already owns it, replies `Ok(false)` without publishing a terminal event.
    async fn remove_task(&self, id: TaskId, reply: oneshot::Sender<RemoveReply>) {
        self.pending_joins.inc(id);
        if let Some((handle, len_after)) = self.take_handle(id).await {
            self.notify_after_remove(len_after);

            handle.cancel.cancel();
            let _ = reply.send(Ok(true));
            self.spawn_join_report(id, handle.label, handle.join, Some(self.grace), handle.done);
        } else {
            self.pending_joins.dec(id);
            let _ = reply.send(Ok(false));
        }
    }

    /// Cleans up a finished actor by identity.
    ///
    /// Called after the actor's reliable completion signal is received.
    /// Duplicate or stale completion signals are no-ops.
    async fn cleanup_task(&self, id: TaskId) {
        self.pending_joins.inc(id);
        if let Some((handle, len_after)) = self.take_handle(id).await {
            self.notify_after_remove(len_after);
            self.spawn_join_report(id, handle.label, handle.join, Some(self.grace), handle.done);
        } else {
            self.pending_joins.dec(id);
        }
    }

    /// Removes a handle and its label index entry atomically.
    ///
    /// Returns the removed handle and the number of registered tasks left.
    async fn take_handle(&self, id: TaskId) -> Option<(Handle, usize)> {
        let mut st = self.state.write().await;
        let h = st.tasks.remove(&id)?;
        st.by_label.remove(&h.label);
        let len_after = st.tasks.len();
        Some((h, len_after))
    }

    /// Joins an actor in a detached task and reports its final result.
    ///
    /// If `force_after` is `Some`, the join is bounded by that duration.
    /// When the actor does not finish in time, it is aborted and watched tasks resolve to [`TaskOutcome::ForceAborted`].
    ///
    /// On normal join, this resolves the optional outcome sender and publishes the final `TaskRemoved`.
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
        pending.label(id, Arc::clone(&name));
        tokio::spawn(async move {
            let mut join = join;
            match force_after {
                Some(grace) => match tokio::time::timeout(grace, &mut join).await {
                    Ok(res) => {
                        Self::report_join(&bus, id, &name, res, done);
                        pending.dec(id);
                    }
                    Err(_) => {
                        join.abort();
                        let _ = join.await;
                        if let Some(done) = done {
                            let _ = done.send(TaskOutcome::ForceAborted);
                        }
                        bus.publish(
                            Event::new(EventKind::TaskRemoved)
                                .with_task(name)
                                .with_id(id)
                                .with_reason("force_terminated_after_grace"),
                        );
                        pending.dec(id);
                    }
                },
                None => {
                    let res = join.await;
                    Self::report_join(&bus, id, &name, res, done);
                    pending.dec(id);
                }
            }
        });
    }

    /// Reports the result of a joined actor.
    ///
    /// Sends the watched [`TaskOutcome`] if present, publishes `ActorDead` for an actor panic, and always publishes `TaskRemoved` for this joined actor.
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

    #[tokio::test]
    async fn pending_wait_drained_resolves_after_last_dec() {
        let p = Arc::new(PendingJoins::default());
        let a = TaskId::next();
        let b = TaskId::next();
        p.inc(a);
        p.inc(b);
        assert!(!p.is_empty());

        let p2 = Arc::clone(&p);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            p2.dec(a);
            p2.dec(b);
        });

        tokio::time::timeout(Duration::from_secs(1), p.wait_drained())
            .await
            .expect("wait_drained must resolve once every join is decremented");
        assert!(p.is_empty(), "no joins should remain after draining");
    }

    #[tokio::test]
    async fn pending_wait_drained_returns_immediately_when_empty() {
        let p = PendingJoins::default();
        tokio::time::timeout(Duration::from_millis(100), p.wait_drained())
            .await
            .expect("an empty PendingJoins must resolve immediately");
    }

    fn registry() -> Arc<Registry> {
        let bus = Bus::new(64);
        let token = CancellationToken::new();
        let (_tx, rx) = mpsc::channel(64);
        Registry::new(bus, token, None, Duration::from_secs(5), rx)
    }

    fn started_registry(
        bus_capacity: usize,
        grace: Duration,
    ) -> (
        Arc<Registry>,
        Bus,
        CancellationToken,
        mpsc::Sender<RegistryCommand>,
    ) {
        let bus = Bus::new(bus_capacity);
        let token = CancellationToken::new();
        let (tx, rx) = mpsc::channel(64);
        let registry = Registry::new(bus.clone(), token.clone(), None, grace, rx);
        registry.clone().spawn_listener();
        (registry, bus, token, tx)
    }

    fn send_add(
        tx: &mpsc::Sender<RegistryCommand>,
        id: TaskId,
        spec: TaskSpec,
        outcome: Option<OutcomeTx>,
    ) -> AddReplyRx {
        let (reply, reply_rx) = oneshot::channel();
        tx.try_send(RegistryCommand::Add {
            id,
            spec,
            outcome,
            reply,
        })
        .expect("registry command channel must stay open");
        reply_rx
    }

    fn send_remove(tx: &mpsc::Sender<RegistryCommand>, id: TaskId) -> RemoveReplyRx {
        let (reply, reply_rx) = oneshot::channel();
        tx.try_send(RegistryCommand::Remove { id, reply })
            .expect("registry command channel must stay open");
        reply_rx
    }

    async fn receive_reply<T>(reply: oneshot::Receiver<T>, name: &str) -> T {
        tokio::time::timeout(Duration::from_secs(2), reply)
            .await
            .unwrap_or_else(|_| panic!("{name} timed out"))
            .unwrap_or_else(|_| panic!("{name} sender was dropped"))
    }

    async fn receive_completion(
        completion_rx: &mut mpsc::UnboundedReceiver<TaskId>,
        name: &str,
    ) -> TaskId {
        tokio::time::timeout(Duration::from_secs(2), completion_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{name} timed out"))
            .unwrap_or_else(|| panic!("{name} channel was closed"))
    }

    async fn stop_registry(registry: &Registry, token: &CancellationToken) {
        token.cancel();
        tokio::time::timeout(Duration::from_secs(2), registry.join_listener())
            .await
            .expect("registry listener must stop");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn add_reply_commits_state_without_event_confirmation() {
        use crate::{TaskContext, TaskFn, TaskRef};
        use tokio::sync::broadcast::error::TryRecvError;

        let (registry, bus, token, tx) = started_registry(1, Duration::from_secs(1));
        let mut stale_events = bus.subscribe();
        let id = TaskId::next();
        let task: TaskRef = TaskFn::arc("reply-add", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });

        let reply = send_add(&tx, id, TaskSpec::restartable(task), None);
        assert!(
            receive_reply(reply, "add reply").await.is_ok(),
            "registry must accept a unique task"
        );
        assert!(
            registry.contains(id).await,
            "reply requires committed id state"
        );
        assert_eq!(
            registry.id_for_label("reply-add").await,
            Some(id),
            "reply requires committed label state"
        );
        assert_eq!(registry.list().await, vec![(id, Arc::from("reply-add"))]);

        for _ in 0..4 {
            bus.publish(Event::new(EventKind::TaskStarting).with_task("noise"));
        }
        assert!(
            matches!(stale_events.try_recv(), Err(TryRecvError::Lagged(_))),
            "the observer must lag in this regression setup"
        );
        assert!(
            registry.contains(id).await,
            "event lag must not change the authoritative add result"
        );

        stop_registry(&registry, &token).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplicate_add_reply_rejects_without_starting_body() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::{TaskContext, TaskFn, TaskRef};

        let (registry, _bus, token, tx) = started_registry(64, Duration::from_secs(1));
        let first_id = TaskId::next();
        let first: TaskRef = TaskFn::arc("duplicate", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        assert!(
            receive_reply(
                send_add(&tx, first_id, TaskSpec::restartable(first), None),
                "first add reply",
            )
            .await
            .is_ok()
        );

        let runs = Arc::new(AtomicUsize::new(0));
        let duplicate_runs = Arc::clone(&runs);
        let duplicate: TaskRef = TaskFn::arc("duplicate", move |_ctx: TaskContext| {
            duplicate_runs.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        });
        let second_id = TaskId::next();
        let (outcome, outcome_rx) = oneshot::channel();
        let duplicate_reply = receive_reply(
            send_add(&tx, second_id, TaskSpec::once(duplicate), Some(outcome)),
            "duplicate add reply",
        )
        .await;

        assert!(
            matches!(
                duplicate_reply,
                Err(RuntimeError::TaskAlreadyExists { name }) if name.as_ref() == "duplicate"
            ),
            "duplicate add must return its authoritative rejection"
        );
        assert!(!registry.contains(second_id).await);
        assert_eq!(registry.id_for_label("duplicate").await, Some(first_id));
        assert_eq!(runs.load(Ordering::SeqCst), 0, "rejected body must not run");
        assert!(matches!(
            receive_reply(outcome_rx, "duplicate outcome").await,
            TaskOutcome::Rejected { reason } if reason.as_ref() == reasons::ALREADY_EXISTS
        ));

        stop_registry(&registry, &token).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn remove_reply_claims_once_before_terminal_completion() {
        use crate::{TaskContext, TaskError, TaskFn, TaskRef};

        let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(1));
        let mut events = bus.subscribe();
        let cancellation_seen = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let seen_by_task = Arc::clone(&cancellation_seen);
        let task_release = Arc::clone(&release);
        let task: TaskRef = TaskFn::arc("remove-once", move |ctx: TaskContext| {
            let seen = Arc::clone(&seen_by_task);
            let release = Arc::clone(&task_release);
            async move {
                ctx.cancelled().await;
                seen.notify_one();
                release.notified().await;
                Err(TaskError::Canceled)
            }
        });
        let id = TaskId::next();
        assert!(
            receive_reply(
                send_add(&tx, id, TaskSpec::restartable(task), None),
                "setup add reply",
            )
            .await
            .is_ok()
        );
        while events.try_recv().is_ok() {}

        assert!(
            matches!(
                receive_reply(send_remove(&tx, id), "first remove reply").await,
                Ok(true)
            ),
            "the first remove must claim the task"
        );
        tokio::time::timeout(Duration::from_secs(2), cancellation_seen.notified())
            .await
            .expect("the task must observe cancellation");
        assert!(registry.pending_joins.contains(id));
        while let Ok(event) = events.try_recv() {
            assert_ne!(
                event.kind,
                EventKind::TaskRemoved,
                "remove reply must not wait for or invent terminal completion"
            );
        }

        assert!(
            matches!(
                receive_reply(send_remove(&tx, id), "second remove reply").await,
                Ok(false)
            ),
            "a second remove cannot claim the same task"
        );

        release.notify_one();
        tokio::time::timeout(
            Duration::from_secs(2),
            registry.pending_joins.wait_drained(),
        )
        .await
        .expect("the released task must finish its join");
        stop_registry(&registry, &token).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_remove_replies_false_without_pending_join() {
        let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(1));
        let mut events = bus.subscribe();
        let unknown = TaskId::next();

        assert!(
            matches!(
                receive_reply(send_remove(&tx, unknown), "unknown remove reply").await,
                Ok(false)
            ),
            "unknown remove must return false"
        );
        assert!(matches!(
            receive_reply(
                send_remove(&tx, TaskId::next()),
                "unknown remove barrier reply",
            )
            .await,
            Ok(false)
        ));
        assert!(
            registry.pending_joins.is_empty(),
            "unknown removal must not leak pending join state"
        );
        assert!(
            std::iter::from_fn(|| events.try_recv().ok())
                .all(|event| event.id != Some(unknown) || event.kind != EventKind::TaskRemoved),
            "unknown removal must not invent a terminal event"
        );

        stop_registry(&registry, &token).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_add_reply_does_not_stop_command_processing() {
        use crate::{TaskContext, TaskFn, TaskRef};

        let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(1));
        let mut events = bus.subscribe();
        let first_id = TaskId::next();
        let first: TaskRef = TaskFn::arc("dropped-add-a", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        drop(send_add(&tx, first_id, TaskSpec::restartable(first), None));

        let second_id = TaskId::next();
        let second: TaskRef = TaskFn::arc("dropped-add-b", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        assert!(
            receive_reply(
                send_add(&tx, second_id, TaskSpec::restartable(second), None),
                "second add reply",
            )
            .await
            .is_ok()
        );

        assert!(registry.contains(first_id).await);
        assert!(registry.contains(second_id).await);
        let mut added = 0;
        while let Ok(event) = events.try_recv() {
            if event.kind == EventKind::TaskAdded {
                added += 1;
            }
        }
        assert_eq!(added, 2, "a dropped reply must not suppress TaskAdded");

        stop_registry(&registry, &token).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_remove_reply_does_not_skip_join_cleanup() {
        use crate::{TaskContext, TaskError, TaskFn, TaskRef};

        let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(1));
        let mut events = bus.subscribe();
        let cancellation_seen = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let seen_by_task = Arc::clone(&cancellation_seen);
        let task_release = Arc::clone(&release);
        let task: TaskRef = TaskFn::arc("dropped-remove", move |ctx: TaskContext| {
            let seen = Arc::clone(&seen_by_task);
            let release = Arc::clone(&task_release);
            async move {
                ctx.cancelled().await;
                seen.notify_one();
                release.notified().await;
                Err(TaskError::Canceled)
            }
        });
        let id = TaskId::next();
        assert!(
            receive_reply(
                send_add(&tx, id, TaskSpec::restartable(task), None),
                "setup add reply",
            )
            .await
            .is_ok()
        );
        while events.try_recv().is_ok() {}

        drop(send_remove(&tx, id));
        assert!(
            matches!(
                receive_reply(
                    send_remove(&tx, TaskId::next()),
                    "synchronizing remove reply",
                )
                .await,
                Ok(false)
            ),
            "the listener must process commands after a dropped reply"
        );
        tokio::time::timeout(Duration::from_secs(2), cancellation_seen.notified())
            .await
            .expect("dropped receiver must not suppress cancellation");

        release.notify_one();
        tokio::time::timeout(
            Duration::from_secs(2),
            registry.pending_joins.wait_drained(),
        )
        .await
        .expect("dropped receiver must not suppress join cleanup");
        let mut saw_removed = false;
        while let Ok(event) = events.try_recv() {
            if event.kind == EventKind::TaskRemoved && event.id == Some(id) {
                saw_removed = true;
            }
        }
        assert!(saw_removed, "join cleanup must still publish TaskRemoved");

        stop_registry(&registry, &token).await;
    }

    #[tokio::test]
    async fn wait_joins_within_reports_stuck_labels_then_drains() {
        let reg = registry();

        assert!(
            reg.wait_joins_within(Duration::from_millis(50))
                .await
                .is_empty(),
            "an empty join set must drain immediately"
        );

        let id = TaskId::next();
        reg.pending_joins.inc(id);
        reg.pending_joins.label(id, Arc::from("stuck-task"));
        let stuck = reg.wait_joins_within(Duration::from_millis(30)).await;
        assert_eq!(
            stuck,
            vec![Arc::<str>::from("stuck-task")],
            "an in-flight join must be reported with its label on timeout"
        );

        let p = Arc::clone(&reg.pending_joins);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            p.dec(id);
        });
        assert!(
            reg.wait_joins_within(Duration::from_secs(1))
                .await
                .is_empty(),
            "must drain once the in-flight join is decremented"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn completion_guard_signals_on_panic_and_abort_before_first_poll() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();

        let panic_id = TaskId::next();
        let panic_handle =
            Registry::spawn_tracked_actor(panic_id, completion_tx.clone(), async move {
                panic!("outer actor panic")
            });
        let panic_result = panic_handle.await;
        assert!(
            panic_result.is_err_and(|error| error.is_panic()),
            "outer actor panic must stay visible through JoinError"
        );
        assert_eq!(
            receive_completion(&mut completion_rx, "panic completion").await,
            panic_id
        );

        let polled = Arc::new(AtomicBool::new(false));
        let polled_by_task = Arc::clone(&polled);
        let abort_id = TaskId::next();
        let abort_handle = Registry::spawn_tracked_actor(abort_id, completion_tx, async move {
            polled_by_task.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
            ActorExitReason::Completed
        });
        abort_handle.abort();
        let abort_result = abort_handle.await;
        assert!(
            abort_result.is_err_and(|error| error.is_cancelled()),
            "aborted actor must return a cancelled JoinError"
        );
        assert!(
            !polled.load(Ordering::SeqCst),
            "the abort regression requires abort-before-first-poll"
        );
        assert_eq!(
            receive_completion(&mut completion_rx, "abort completion").await,
            abort_id
        );
        assert!(
            completion_rx.try_recv().is_err(),
            "each actor exit must send one completion identity"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn natural_completion_cleans_registry_when_event_observer_lags() {
        use crate::{TaskContext, TaskFn, TaskRef};
        use tokio::sync::broadcast::error::TryRecvError;

        let (registry, bus, token, tx) = started_registry(1, Duration::from_secs(1));
        let mut stale_events = bus.subscribe();
        let task: TaskRef = TaskFn::arc("completion-no-bus", |_ctx: TaskContext| async { Ok(()) });
        let id = TaskId::next();
        let (outcome, outcome_rx) = oneshot::channel();

        assert!(
            receive_reply(
                send_add(&tx, id, TaskSpec::once(task), Some(outcome)),
                "fast add reply",
            )
            .await
            .is_ok()
        );
        tokio::time::timeout(Duration::from_secs(2), registry.wait_until_empty())
            .await
            .expect("completion channel must remove the finished task");
        assert!(
            registry
                .wait_joins_within(Duration::from_secs(2))
                .await
                .is_empty()
        );
        assert!(matches!(
            receive_reply(outcome_rx, "fast task outcome").await,
            TaskOutcome::Completed
        ));
        assert_eq!(registry.id_for_label("completion-no-bus").await, None);
        assert!(
            matches!(stale_events.try_recv(), Err(TryRecvError::Lagged(_))),
            "the observer must lose terminal events in this regression setup"
        );

        stop_registry(&registry, &token).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn forged_terminal_event_does_not_remove_running_actor() {
        use crate::{TaskContext, TaskFn, TaskRef};

        let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(1));
        let _observer = bus.subscribe();
        assert_eq!(
            bus.receiver_count(),
            1,
            "the registry listener must not subscribe to the event bus"
        );
        let id = TaskId::next();
        let task: TaskRef = TaskFn::arc("ignore-terminal-event", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        assert!(
            receive_reply(
                send_add(&tx, id, TaskSpec::restartable(task), None),
                "running add reply",
            )
            .await
            .is_ok()
        );

        bus.publish(
            Event::new(EventKind::ActorExhausted)
                .with_task("ignore-terminal-event")
                .with_id(id),
        );

        let barrier_id = TaskId::next();
        let barrier: TaskRef = TaskFn::arc("event-barrier", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        assert!(
            receive_reply(
                send_add(&tx, barrier_id, TaskSpec::restartable(barrier), None,),
                "barrier add reply",
            )
            .await
            .is_ok()
        );
        assert!(
            registry.contains(id).await,
            "terminal events are observability and cannot trigger cleanup"
        );

        stop_registry(&registry, &token).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn outer_actor_panic_is_reaped_by_completion_channel() {
        let (registry, bus, token, _tx) = started_registry(64, Duration::from_secs(1));
        let mut events = bus.subscribe();
        let id = TaskId::next();
        let label: Arc<str> = Arc::from("outer-panic");
        let (done, done_rx) = oneshot::channel();

        let mut state = registry.state.write().await;
        let join = Registry::spawn_tracked_actor(id, registry.completion_tx.clone(), async move {
            panic!("outer actor panic")
        });
        state.by_label.insert(Arc::clone(&label), id);
        state.tasks.insert(
            id,
            Handle {
                join,
                cancel: CancellationToken::new(),
                label: Arc::clone(&label),
                done: Some(done),
            },
        );
        drop(state);

        assert!(matches!(
            receive_reply(done_rx, "panic outcome").await,
            TaskOutcome::Panicked
        ));
        tokio::time::timeout(Duration::from_secs(2), registry.wait_until_empty())
            .await
            .expect("panicked actor must leave the registry");
        assert!(
            registry
                .wait_joins_within(Duration::from_secs(2))
                .await
                .is_empty()
        );

        let mut actor_dead = 0;
        let mut task_removed = 0;
        while let Ok(event) = events.try_recv() {
            if event.id == Some(id) && event.kind == EventKind::ActorDead {
                actor_dead += 1;
            }
            if event.id == Some(id) && event.kind == EventKind::TaskRemoved {
                task_removed += 1;
            }
        }
        assert_eq!(actor_dead, 1);
        assert_eq!(task_removed, 1);

        stop_registry(&registry, &token).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn remove_path_owns_cleanup_when_completion_signal_arrives() {
        use crate::{TaskContext, TaskError, TaskFn, TaskRef};

        let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(1));
        let mut events = bus.subscribe();
        let cancellation_seen = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let seen_by_task = Arc::clone(&cancellation_seen);
        let task_release = Arc::clone(&release);
        let task: TaskRef = TaskFn::arc("remove-completion-race", move |ctx: TaskContext| {
            let seen = Arc::clone(&seen_by_task);
            let release = Arc::clone(&task_release);
            async move {
                ctx.cancelled().await;
                seen.notify_one();
                release.notified().await;
                Err(TaskError::Canceled)
            }
        });
        let id = TaskId::next();
        let (done, done_rx) = oneshot::channel();
        assert!(
            receive_reply(
                send_add(&tx, id, TaskSpec::restartable(task), Some(done),),
                "race add reply",
            )
            .await
            .is_ok()
        );
        while events.try_recv().is_ok() {}

        assert!(matches!(
            receive_reply(send_remove(&tx, id), "race remove reply").await,
            Ok(true)
        ));
        tokio::time::timeout(Duration::from_secs(2), cancellation_seen.notified())
            .await
            .expect("removed task must observe cancellation");
        release.notify_one();
        assert!(matches!(
            receive_reply(done_rx, "race outcome").await,
            TaskOutcome::Canceled
        ));
        tokio::time::timeout(
            Duration::from_secs(2),
            registry.pending_joins.wait_drained(),
        )
        .await
        .expect("remove-owned join must drain");

        assert!(matches!(
            receive_reply(send_remove(&tx, TaskId::next()), "completion barrier reply",).await,
            Ok(false)
        ));
        let removed_count = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|event| event.id == Some(id) && event.kind == EventKind::TaskRemoved)
            .count();
        assert_eq!(
            removed_count, 1,
            "stale completion signal must not duplicate terminal cleanup"
        );

        stop_registry(&registry, &token).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn completion_claim_before_remove_emits_one_terminal_event() {
        use crate::{TaskContext, TaskFn, TaskRef};

        let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(1));
        let mut events = bus.subscribe();
        let release = Arc::new(Notify::new());
        let task_release = Arc::clone(&release);
        let task: TaskRef = TaskFn::arc("completion-first", move |_ctx: TaskContext| {
            let release = Arc::clone(&task_release);
            async move {
                release.notified().await;
                Ok(())
            }
        });
        let id = TaskId::next();
        let (done, done_rx) = oneshot::channel();
        assert!(
            receive_reply(
                send_add(&tx, id, TaskSpec::once(task), Some(done)),
                "completion-first add reply",
            )
            .await
            .is_ok()
        );
        while events.try_recv().is_ok() {}

        registry
            .completion_tx
            .send(id)
            .expect("completion receiver must be open");
        tokio::time::timeout(Duration::from_secs(2), registry.wait_until_empty())
            .await
            .expect("completion branch must claim the handle");
        assert!(registry.pending_joins.contains(id));
        assert!(matches!(
            receive_reply(send_remove(&tx, id), "completion-first remove reply").await,
            Ok(false)
        ));
        assert!(
            std::iter::from_fn(|| events.try_recv().ok())
                .all(|event| event.id != Some(id) || event.kind != EventKind::TaskRemoved),
            "remove must not report termination while cleanup is still joining"
        );

        release.notify_one();
        assert!(matches!(
            receive_reply(done_rx, "completion-first outcome").await,
            TaskOutcome::Completed
        ));
        tokio::time::timeout(
            Duration::from_secs(2),
            registry.pending_joins.wait_drained(),
        )
        .await
        .expect("completion-owned join must drain");

        assert!(matches!(
            receive_reply(
                send_remove(&tx, TaskId::next()),
                "duplicate completion barrier reply",
            )
            .await,
            Ok(false)
        ));
        let removed_count = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|event| event.id == Some(id) && event.kind == EventKind::TaskRemoved)
            .count();
        assert_eq!(
            removed_count, 1,
            "completion-first race must publish one terminal event"
        );

        stop_registry(&registry, &token).await;
    }

    #[tokio::test]
    async fn shutdown_drains_buffered_command_and_never_silently_drops() {
        use crate::{TaskContext, TaskError, TaskFn, TaskRef};

        let bus = Bus::new(64);
        let token = CancellationToken::new();
        let (tx, rx) = mpsc::channel(1);
        let reg = Registry::new(bus, token.clone(), None, Duration::from_millis(50), rx);

        let task: TaskRef = TaskFn::arc("buffered", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Err(TaskError::Canceled)
        });
        let (done_tx, done_rx) = oneshot::channel();
        let (reply_tx, reply_rx) = oneshot::channel();
        let id = TaskId::next();
        tx.try_send(RegistryCommand::Add {
            id,
            spec: TaskSpec::restartable(task),
            outcome: Some(done_tx),
            reply: reply_tx,
        })
        .expect("channel is open before shutdown");

        token.cancel();
        reg.clone().spawn_listener();
        tokio::time::timeout(Duration::from_secs(2), reg.join_listener())
            .await
            .expect("join_listener must not hang");

        let reply = tokio::time::timeout(Duration::from_secs(1), reply_rx)
            .await
            .expect("buffered Add reply must resolve")
            .expect("buffered Add reply sender must not be dropped");
        assert!(
            reply.is_ok(),
            "buffered Add must be registered before drain"
        );

        let outcome = tokio::time::timeout(Duration::from_secs(1), done_rx)
            .await
            .expect("watcher must resolve")
            .expect("watcher sender must not be dropped — the buffered Add must be acted on");
        assert!(
            matches!(outcome, TaskOutcome::Canceled | TaskOutcome::ForceAborted),
            "a buffered task drained at shutdown must terminate, got {outcome:?}"
        );

        assert!(
            reg.pending_joins.is_empty(),
            "wait_drained must leave no in-flight joins after shutdown"
        );

        let (reply, _reply_rx) = oneshot::channel();
        assert!(
            tx.try_send(RegistryCommand::Remove {
                id: TaskId::next(),
                reply,
            })
            .is_err(),
            "after shutdown the command channel is closed; sends must return Err"
        );
    }
}
