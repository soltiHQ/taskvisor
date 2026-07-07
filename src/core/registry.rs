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
//!   SupervisorCore -> mpsc -> RegistryCommand::Add / Remove
//!
//! Event plane:
//!   TaskActor -> broadcast bus -> ActorExhausted / ActorDead -> cleanup
//! ```
//!
//! Add and remove commands use the management channel, not the lossy event bus.
//! Actor terminal events arrive through the bus and trigger registry cleanup.
//!
//! ## Flow
//!
//! ```text
//! Add(id, spec)
//!   -> spawn actor
//!   -> insert id/name indexes
//!   -> publish TaskAdded
//!
//! Remove(id)
//!   -> remove handle from registry
//!   -> cancel actor token
//!   -> join actor
//!   -> publish TaskRemoved
//!
//! ActorExhausted(id) / ActorDead(id)
//!   -> remove handle from registry
//!   -> join actor
//!   -> publish TaskRemoved
//! ```
//!
//! ## Rules
//!
//! - Watched tasks resolve their `TaskOutcome` when the actor join is reported.
//! - Cleanup is idempotent. Duplicate or stale terminal events become no-ops.
//! - The registry does not clean up on `TaskStopped` or `TaskFailed`.
//! - Task name is a human label and a duplicate-name admission gate.
//!   Cleanup waits for actor-level terminal events.
//! - `TaskRemoved` means the registry has finished cleanup for that identity.
//! - `TaskId` is the canonical identity.

use std::{collections::HashMap, future::Future, sync::Arc, time::Duration};

use tokio::sync::{Notify, RwLock, Semaphore, mpsc, oneshot};
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::core::actor::{ActorExitReason, TaskActor, TaskActorParams};
use crate::core::outcome::TaskOutcome;
use crate::events::{Bus, Event, EventKind};
use crate::identity::TaskId;
use crate::reasons;
use crate::tasks::TaskSpec;

/// Sender used to resolve a watched task with its final [`TaskOutcome`].
pub(crate) type OutcomeTx = oneshot::Sender<TaskOutcome>;

/// Command sent to the registry over the management channel.
pub(crate) enum RegistryCommand {
    /// Register a task under a pre-minted runtime identity.
    Add(TaskId, TaskSpec, Option<OutcomeTx>),
    /// Remove a task by runtime identity.
    ///
    /// The public caller publishes `TaskRemoveRequested` before sending this.
    Remove(TaskId),
}

/// Registry-owned actor handle for one registered task.
struct Handle {
    join: JoinHandle<ActorExitReason>,
    cancel: CancellationToken,
    label: Arc<str>,
    done: Option<OutcomeTx>,
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
/// The registry accepts add/remove commands, listens for actor terminal events, joins actors after removal or completion, and publishes
/// registry-level lifecycle events such as `TaskAdded` and `TaskRemoved`.
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
    cmd_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<RegistryCommand>>>,
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
    /// The listener consumes the command receiver stored during construction.
    /// It listens to:
    /// - management commands from `cmd_rx`,
    /// - actor terminal events from the broadcast bus.
    ///
    /// On runtime shutdown, it closes the command receiver, drains already buffered commands, cancels remaining actors with zero extra grace, and waits for all join reporters to finish.
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

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;

                    _ = rt.cancelled() => break,

                    cmd = cmd_rx.recv() => match cmd {
                        Some(RegistryCommand::Add(id, spec, done)) => {
                            me.guarded("registry", me.spawn_and_register(id, spec, done))
                            .await;
                        }
                        Some(RegistryCommand::Remove(id)) => {
                            me.guarded("registry", me.remove_task(id)).await;
                        }
                        None => break,
                    },

                    msg = bus_rx.recv() => match msg {
                        Ok(ev) => me.guarded("registry", me.handle_bus_event(&ev)).await,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            me.bus
                                .publish(Event::subscriber_overflow("registry", format!("lagged({n})")));
                            me.guarded("registry", me.reap_finished()).await;
                            continue;
                        }
                    }
                }
            }

            cmd_rx.close();
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    RegistryCommand::Add(id, spec, done) => {
                        me.guarded("registry", me.spawn_and_register(id, spec, done))
                            .await;
                    }
                    RegistryCommand::Remove(id) => {
                        me.guarded("registry", me.remove_task(id)).await;
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

    /// Handles registry-relevant lifecycle events from the bus.
    ///
    /// Only actor terminal events trigger cleanup.
    /// Attempt-level events such as `TaskStopped` or `TaskFailed` are ignored here.
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

    /// Spawns an actor and registers it under `id`.
    ///
    /// Duplicate task names are rejected.
    /// If a watched add includes `done`, it resolves as [`TaskOutcome::Rejected`]
    /// with reason [`ALREADY_EXISTS`](crate::reasons::ALREADY_EXISTS).
    ///
    /// Direct `add_and_watch` callers still receive [`RuntimeError::TaskAlreadyExists`](crate::RuntimeError::TaskAlreadyExists) because registration confirmation fails before the waiter is returned.
    async fn spawn_and_register(&self, id: TaskId, spec: TaskSpec, done: Option<OutcomeTx>) {
        let label: Arc<str> = Arc::from(spec.task().name());

        let mut st = self.state.write().await;
        if st.by_label.contains_key(&label) {
            drop(st);
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
    ///
    /// If the task exists, its actor token is cancelled and a detached join reporter publishes the final `TaskRemoved`.
    ///
    /// If the task is unknown, publishes `TaskRemoved` with reason `task_not_found`.
    async fn remove_task(&self, id: TaskId) {
        self.pending_joins.inc(id);
        if let Some((handle, len_after)) = self.take_handle(id).await {
            self.notify_after_remove(len_after);

            handle.cancel.cancel();
            self.spawn_join_report(id, handle.label, handle.join, Some(self.grace), handle.done);
        } else {
            self.pending_joins.dec(id);
            self.bus.publish(
                Event::new(EventKind::TaskRemoved)
                    .with_id(id)
                    .with_reason("task_not_found"),
            );
        }
    }

    /// Cleans up a finished actor by identity.
    ///
    /// Called after `ActorExhausted` or `ActorDead`.
    /// Duplicate/stale cleanup events are no-ops.
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

    /// Reaps actors whose join handle has already finished.
    ///
    /// Used as recovery after broadcast lag, when an actor terminal event may have been skipped by the registry listener.
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
        let (_tx, rx) = mpsc::unbounded_channel();
        Registry::new(bus, token, None, Duration::from_secs(5), rx)
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

    #[tokio::test]
    async fn shutdown_drains_buffered_command_and_never_silently_drops() {
        use crate::{TaskContext, TaskError, TaskFn, TaskRef};

        let bus = Bus::new(64);
        let token = CancellationToken::new();
        let (tx, rx) = mpsc::unbounded_channel();
        let reg = Registry::new(bus, token.clone(), None, Duration::from_millis(50), rx);
        reg.clone().spawn_listener();

        let task: TaskRef = TaskFn::arc("buffered", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Err(TaskError::Canceled)
        });
        let (done_tx, done_rx) = oneshot::channel();
        let id = TaskId::next();
        tx.send(RegistryCommand::Add(
            id,
            TaskSpec::restartable(task),
            Some(done_tx),
        ))
        .expect("channel is open before shutdown");

        token.cancel();
        tokio::time::timeout(Duration::from_secs(2), reg.join_listener())
            .await
            .expect("join_listener must not hang");

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

        assert!(
            tx.send(RegistryCommand::Remove(TaskId::next())).is_err(),
            "after shutdown the command channel is closed; sends must return Err"
        );
    }
}
