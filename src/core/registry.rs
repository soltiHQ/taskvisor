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
//!   SupervisorCore                  Registry
//!        |                             |
//!        |-- command (bounded mpsc) -->|
//!        |                             |-- update registry state
//!        |<-- reply (oneshot) ---------|
//!        |-- fence (control channel) ->|  drain committed commands
//!        |<-- fence reply -------------|
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
//!   -> create shared terminal completion
//!   -> spawn actor
//!   -> insert id/name indexes
//!   -> reply registered
//!   -> publish TaskAdded
//!
//! Remove(id)
//!   -> change Registered to Removing
//!   -> reuse shared terminal completion
//!   -> cancel actor token
//!   -> reply claimed
//!   -> join actor
//!   -> release id/name indexes
//!   -> publish TaskRemoved
//!   -> resolve terminal completion
//!
//! Cancel(id or label)
//!   -> claim Registered or join Removing
//!   -> reply with the shared terminal completion
//!   -> wait until registry cleanup is finished
//!
//! Actor completion(id)
//!   -> change Registered to Removing
//!   -> reuse shared terminal completion
//!   -> join actor
//!   -> release id/name indexes
//!   -> publish TaskRemoved
//!   -> resolve terminal completion
//! ```
//!
//! ## Rules
//!
//! - Watched tasks resolve their `TaskOutcome` when the actor join is reported.
//! - Add/remove command replies report registry decisions, not terminal task outcomes.
//! - Cancel callers and controller slots share a lossless completion signal owned by the registry.
//! - A fence reply means every earlier management command has finished processing.
//! - A static AddBatch validates every label before it starts any task body.
//! - AddBatch either registers every item or leaves registry state unchanged.
//! - Cleanup is idempotent. Duplicate or stale completion signals become no-ops.
//! - The registry does not clean up on `TaskStopped` or `TaskFailed`.
//! - Task name is a label and a duplicate-name admission gate; reserved while the task is `Registered` or `Removing`.
//! - `list`, identity lookup, and empty checks include removing tasks.
//! - Cleanup waits for the actor completion signal and join result.
//! - `TaskRemoved` means the registry has finished cleanup for that identity.
//! - `TaskId` is the canonical identity.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::Arc,
    time::Duration,
};

use tokio::sync::{Notify, RwLock, Semaphore, mpsc, oneshot, watch};
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::core::TaskDefaults;
use crate::core::actor::{ActorExitReason, TaskActor, TaskActorParams};
use crate::core::outcome::TaskOutcome;
use crate::error::RuntimeError;
use crate::events::{Bus, Event, EventKind};
use crate::identity::TaskId;
use crate::reasons;
use crate::tasks::TaskSpec;

/// Sender used to resolve a watched task with its final [`TaskOutcome`].
pub(crate) type OutcomeTx = oneshot::Sender<TaskOutcome>;

/// Authoritative result of one single-task or batch registry add command.
pub(crate) type AddReply = Result<(), RuntimeError>;

/// Receiver for an authoritative registry add result.
pub(crate) type AddReplyRx = oneshot::Receiver<AddReply>;

/// One task owned by the atomic static-run registration command.
pub(crate) struct AddBatchItem {
    pub(crate) id: TaskId,
    pub(crate) label: Arc<str>,
    pub(crate) spec: TaskSpec,
}

/// Authoritative result of one registry remove command.
///
/// `Ok(true)` means the registry claimed the task and sent cancellation.
/// It does not mean the actor has terminated yet.
pub(crate) type RemoveReply = Result<bool, RuntimeError>;

/// Receiver for an authoritative registry remove result.
pub(crate) type RemoveReplyRx = oneshot::Receiver<RemoveReply>;

/// Registry decision returned to one cancellation caller.
///
/// `claimed` is true only for the caller that changed `Registered` to `Removing`.
/// Every caller that observes the same removal waits on the same terminal completion.
pub(crate) struct CancelDecision {
    pub(crate) id: TaskId,
    pub(crate) claimed: bool,
    completion: RemovalCompletion,
}

impl CancelDecision {
    /// Waits until the actor is joined or force-aborted and terminal cleanup is committed.
    pub(crate) async fn wait(&self) {
        self.completion.wait().await;
    }

    /// Returns true when terminal cleanup has already been committed.
    pub(crate) fn is_complete(&self) -> bool {
        self.completion.is_complete()
    }
}

/// Authoritative result of one registry cancel command.
///
/// `Ok(None)` means the task was unknown or already terminated.
pub(crate) type CancelReply = Result<Option<CancelDecision>, RuntimeError>;

/// Receiver for an authoritative registry cancel decision.
pub(crate) type CancelReplyRx = oneshot::Receiver<CancelReply>;

/// Command sent to the registry over the management channel.
pub(crate) enum RegistryCommand {
    /// Register a task under a pre-minted runtime identity.
    Add {
        id: TaskId,
        spec: TaskSpec,
        outcome: Option<OutcomeTx>,
        completion: Option<RemovalCompletion>,
        reply: oneshot::Sender<AddReply>,
    },
    /// Validate and register every static-run task as one operation.
    AddBatch {
        items: Vec<AddBatchItem>,
        reply: oneshot::Sender<AddReply>,
    },
    /// Remove a task by runtime identity.
    ///
    /// The identity caller publishes `TaskRemoveRequested` before sending this.
    Remove {
        id: TaskId,
        reply: oneshot::Sender<RemoveReply>,
    },
    /// Resolve a label and claim its current owner in one registry operation.
    ///
    /// The registry publishes `TaskRemoveRequested` with the resolved identity before it attempts the state transition.
    RemoveByLabel {
        label: Arc<str>,
        reply: oneshot::Sender<RemoveReply>,
    },
    /// Claim or join cancellation by runtime identity.
    Cancel {
        id: TaskId,
        reply: oneshot::Sender<CancelReply>,
    },
    /// Resolve a label and claim or join its cancellation atomically.
    CancelByLabel {
        label: Arc<str>,
        reply: oneshot::Sender<CancelReply>,
    },
}

/// Reliable control messages that must not wait for management queue capacity.
enum RegistryControl {
    /// Confirms that every command committed before admission closed has been processed.
    Fence { reply: oneshot::Sender<()> },
}

/// Registry-owned actor handle for one registered task.
struct Handle {
    join: JoinHandle<ActorExitReason>,
    cancel: CancellationToken,
    done: Option<OutcomeTx>,
    completion: RemovalCompletion,
}

/// Shared terminal signal for callers waiting until registry cleanup is committed.
#[derive(Clone, Debug)]
pub(crate) struct RemovalCompletion {
    token: CancellationToken,
}

impl RemovalCompletion {
    pub(crate) fn new() -> Self {
        Self {
            token: CancellationToken::new(),
        }
    }

    pub(crate) async fn wait(&self) {
        self.token.cancelled().await;
    }

    fn is_complete(&self) -> bool {
        self.token.is_cancelled()
    }

    fn complete(&self) {
        self.token.cancel();
    }
}

/// Lifecycle phase of one authoritative registry entry.
enum EntryState {
    /// The actor can still be claimed by remove, completion, or shutdown.
    Registered(Handle),
    /// One owner has the actor handle and is waiting for its terminal join.
    Removing { completion: RemovalCompletion },
}

/// Authoritative membership record kept until terminal join cleanup finishes.
struct Entry {
    label: Arc<str>,
    state: EntryState,
}

/// Terminal result passed from the single join owner to registry cleanup.
enum JoinCompletion {
    Joined(Result<ActorExitReason, JoinError>),
    ForceAborted,
}

/// Data needed to commit one actor's terminal registry cleanup.
struct RemovalReport {
    id: TaskId,
    outcome: Option<OutcomeTx>,
    join: JoinCompletion,
    completion: RemovalCompletion,
}

/// Registry-side work selected for one cancel command.
struct CancelAction {
    decision: CancelDecision,
    handle: Option<Handle>,
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
    ///
    /// Entries stay here in both `Registered` and `Removing` phases.
    tasks: HashMap<TaskId, Entry>,

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

/// Tracks actor joins owned by removing entries.
///
/// This provides shutdown diagnostics and a wait barrier while the registry map remains the authority for task membership.
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
    #[cfg(test)]
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
            tokio::pin!(notified);
            notified.as_mut().enable();
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
    state: Arc<RwLock<Inner>>,
    bus: Bus,
    runtime_token: CancellationToken,
    semaphore: Option<Arc<Semaphore>>,
    grace: Duration,
    task_defaults: TaskDefaults,
    empty_notify: Arc<Notify>,
    cmd_rx: std::sync::Mutex<Option<mpsc::Receiver<RegistryCommand>>>,
    control_tx: mpsc::UnboundedSender<RegistryControl>,
    control_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<RegistryControl>>>,
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
        task_defaults: TaskDefaults,
        cmd_rx: mpsc::Receiver<RegistryCommand>,
    ) -> Arc<Self> {
        let (completion_tx, completion_rx) = mpsc::unbounded_channel();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        Arc::new(Self {
            state: Arc::new(RwLock::new(Inner::default())),
            bus,
            runtime_token,
            semaphore,
            grace,
            task_defaults,
            empty_notify: Arc::new(Notify::new()),
            cmd_rx: std::sync::Mutex::new(Some(cmd_rx)),
            control_tx,
            control_rx: std::sync::Mutex::new(Some(control_rx)),
            completion_tx,
            completion_rx: std::sync::Mutex::new(Some(completion_rx)),
            pending_joins: Arc::new(PendingJoins::default()),
            listener_handle: std::sync::Mutex::new(None),
        })
    }

    /// Waits for detached join reporters to finish.
    ///
    /// Join reporters own actor handles for entries in the `Removing` phase.
    /// Shutdown still needs their final `TaskRemoved` events before the subscriber listener stops.
    ///
    /// Returns labels for joins still in flight after `grace`.
    pub async fn wait_joins_within(&self, grace: Duration) -> Vec<Arc<str>> {
        let _ = tokio::time::timeout(grace, self.pending_joins.wait_drained()).await;
        self.pending_joins.pending_labels()
    }

    /// Waits until no registered or removing tasks remain.
    ///
    /// Uses register-before-check to avoid losing a wakeup.
    pub async fn wait_until_empty(&self) {
        loop {
            let notified = self.empty_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_empty().await {
                return;
            }
            notified.await;
        }
    }

    /// Waits until every management command committed before this call has been processed.
    ///
    /// The control channel is independent of bounded management queue capacity.
    pub(crate) async fn fence(&self) -> Result<(), RuntimeError> {
        let (reply, reply_rx) = oneshot::channel();
        self.control_tx
            .send(RegistryControl::Fence { reply })
            .map_err(|_| RuntimeError::ShuttingDown)?;
        reply_rx.await.map_err(|_| RuntimeError::ShuttingDown)
    }

    /// Starts the registry listener task.
    ///
    /// The listener consumes receivers stored during construction.
    /// It listens to:
    /// - management commands from `cmd_rx`,
    /// - shutdown fences from the independent control channel,
    /// - actor identity signals from the reliable completion channel.
    ///
    /// On runtime shutdown, it closes the command receiver, drains commands that
    /// are already buffered without waiting for uncommitted reservations, cancels
    /// remaining actors with zero extra grace, and waits for join reporters.
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
        let mut control_rx = self
            .control_rx
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

                    control = control_rx.recv() => match control {
                        Some(control) => me.handle_control(control, &mut cmd_rx).await,
                        None => break,
                    },

                    cmd = cmd_rx.recv() => match cmd {
                        Some(command) => me.handle_command(command).await,
                        None => break,
                    }
                }
            }

            cmd_rx.close();
            completion_rx.close();
            control_rx.close();
            while let Ok(cmd) = cmd_rx.try_recv() {
                me.handle_command(cmd).await;
            }
            while let Ok(control) = control_rx.try_recv() {
                me.handle_control(control, &mut cmd_rx).await;
            }
            me.cancel_all_within(Duration::ZERO).await;
            me.pending_joins.wait_drained().await;
        });

        *self
            .listener_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    /// Processes one management command to its direct registry decision.
    async fn handle_command(&self, command: RegistryCommand) {
        match command {
            RegistryCommand::Add {
                id,
                spec,
                outcome,
                completion,
                reply,
            } => {
                self.guarded(
                    "registry",
                    self.spawn_and_register(id, spec, outcome, completion, reply),
                )
                .await;
            }
            RegistryCommand::AddBatch { items, reply } => {
                self.guarded("registry", self.spawn_and_register_batch(items, reply))
                    .await;
            }
            RegistryCommand::Remove { id, reply } => {
                self.guarded("registry", self.remove_task(id, reply)).await;
            }
            RegistryCommand::RemoveByLabel { label, reply } => {
                self.guarded("registry", self.remove_task_by_label(label, reply))
                    .await;
            }
            RegistryCommand::Cancel { id, reply } => {
                self.guarded("registry", self.cancel_task(id, reply)).await;
            }
            RegistryCommand::CancelByLabel { label, reply } => {
                self.guarded("registry", self.cancel_task_by_label(label, reply))
                    .await;
            }
        }
    }

    /// Drains commands already visible at the admission ordering point, then replies.
    async fn handle_control(
        &self,
        control: RegistryControl,
        cmd_rx: &mut mpsc::Receiver<RegistryCommand>,
    ) {
        match control {
            RegistryControl::Fence { reply } => {
                while let Ok(command) = cmd_rx.try_recv() {
                    self.handle_command(command).await;
                }
                let _ = reply.send(());
            }
        }
    }

    /// Waits for the registry listener task to finish.
    ///
    /// Safe to call after shutdown has started.
    /// If the listener was never started, this is a no-op.
    /// Returns `false` when Tokio reports that the listener did not join cleanly.
    pub async fn join_listener(&self) -> bool {
        let handle = self
            .listener_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some(handle) = handle else {
            return true;
        };

        match handle.await {
            Ok(()) => true,
            Err(error) => {
                self.bus.publish(Event::subscriber_panicked(
                    "registry",
                    format!("listener join failed: {error}"),
                ));
                false
            }
        }
    }

    /// Aborts the listener so shutdown join-failure handling can be tested.
    #[cfg(test)]
    pub(crate) fn abort_listener_for_test(&self) {
        if let Some(handle) = self
            .listener_handle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            handle.abort();
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

    /// Returns registered and removing tasks as `(id, label)` pairs, sorted by identity.
    pub async fn list(&self) -> Vec<(TaskId, Arc<str>)> {
        let st = self.state.read().await;
        let mut v: Vec<(TaskId, Arc<str>)> = st
            .tasks
            .iter()
            .map(|(id, entry)| (*id, Arc::clone(&entry.label)))
            .collect();
        v.sort_by_key(|(id, _)| *id);
        v
    }

    /// Returns true if `id` is registered or removing.
    #[cfg(test)]
    pub async fn contains(&self, id: TaskId) -> bool {
        self.state.read().await.tasks.contains_key(&id)
    }

    /// Resolves a label to the identity currently holding it (if any).
    #[cfg(test)]
    pub async fn id_for_label(&self, name: &str) -> Option<TaskId> {
        self.state.read().await.by_label.get(name).copied()
    }

    /// Returns true if no tasks are registered or removing.
    pub async fn is_empty(&self) -> bool {
        self.state.read().await.tasks.is_empty()
    }

    /// Cancels all registered tasks and waits for them within one shared grace window.
    ///
    /// Steps:
    /// - change every `Registered` entry to `Removing`,
    /// - cancel every actor token,
    /// - join each actor until the shared deadline,
    /// - abort actors that do not finish in time,
    /// - publish `TaskRemoved` for each drained task.
    ///
    /// Returns labels of tasks that were force-aborted.
    pub async fn cancel_all_within(&self, grace: Duration) -> Vec<Arc<str>> {
        let grace = grace.min(Duration::from_secs(60 * 60 * 24 * 365 * 30));
        let handles: Vec<(TaskId, Arc<str>, Handle, RemovalCompletion)> = {
            let mut st = self.state.write().await;
            let ids: Vec<TaskId> = st.tasks.keys().copied().collect();
            ids.into_iter()
                .filter_map(|id| {
                    Self::claim_registered(&mut st, &self.pending_joins, id)
                        .map(|(label, handle, completion)| (id, label, handle, completion))
                })
                .collect()
        };
        for (_, _, h, _) in &handles {
            h.cancel.cancel();
        }

        let deadline = tokio::time::Instant::now() + grace;
        let mut stuck = Vec::new();

        for (id, label, h, removal_completion) in handles {
            let mut join = h.join;
            match tokio::time::timeout_at(deadline, &mut join).await {
                Ok(res) => {
                    Self::finish_removal(
                        &self.state,
                        &self.empty_notify,
                        &self.pending_joins,
                        &self.bus,
                        RemovalReport {
                            id,
                            outcome: h.done,
                            join: JoinCompletion::Joined(res),
                            completion: removal_completion,
                        },
                    )
                    .await;
                }
                Err(_elapsed) => {
                    join.abort();
                    let _ = join.await;
                    stuck.push(Arc::clone(&label));
                    Self::finish_removal(
                        &self.state,
                        &self.empty_notify,
                        &self.pending_joins,
                        &self.bus,
                        RemovalReport {
                            id,
                            outcome: h.done,
                            join: JoinCompletion::ForceAborted,
                            completion: removal_completion,
                        },
                    )
                    .await;
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

    /// Spawns one registered actor, optionally held behind a batch start gate.
    fn spawn_entry(
        &self,
        id: TaskId,
        label: Arc<str>,
        spec: TaskSpec,
        done: Option<OutcomeTx>,
        completion: Option<RemovalCompletion>,
        start: Option<watch::Receiver<bool>>,
    ) -> Entry {
        let task_token = self.runtime_token.child_token();
        let spec = spec.resolve(&self.task_defaults);

        let actor = TaskActor::new(
            self.bus.clone(),
            Arc::clone(&label),
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
        let actor_future = async move {
            if let Some(mut start) = start {
                loop {
                    if *start.borrow_and_update() {
                        break;
                    }
                    if start.changed().await.is_err() {
                        return ActorExitReason::Canceled;
                    }
                }
            }
            actor.run(task_token_clone).await
        };
        let join_handle = Self::spawn_tracked_actor(id, self.completion_tx.clone(), actor_future);

        Entry {
            label,
            state: EntryState::Registered(Handle {
                join: join_handle,
                cancel: task_token,
                done,
                completion: completion.unwrap_or_else(RemovalCompletion::new),
            }),
        }
    }

    /// Validates and registers a complete static task batch without partial start.
    async fn spawn_and_register_batch(
        &self,
        items: Vec<AddBatchItem>,
        reply: oneshot::Sender<AddReply>,
    ) {
        let mut st = self.state.write().await;
        let mut seen = HashSet::with_capacity(items.len());
        let mut conflicting_ids = HashSet::new();
        let mut first_conflict = None;

        for item in &items {
            let conflicts_with_registry = st.by_label.contains_key(&item.label);
            let repeats_in_batch = !seen.insert(Arc::clone(&item.label));
            if conflicts_with_registry || repeats_in_batch {
                first_conflict.get_or_insert_with(|| Arc::clone(&item.label));
                conflicting_ids.insert(item.id);
            }
        }

        if let Some(name) = first_conflict {
            drop(st);
            for item in items {
                let reason = if conflicting_ids.contains(&item.id) {
                    reasons::ALREADY_EXISTS
                } else {
                    reasons::BATCH_REJECTED
                };
                self.bus.publish(
                    Event::new(EventKind::TaskAddFailed)
                        .with_task(item.label)
                        .with_id(item.id)
                        .with_reason(reason),
                );
            }
            let _ = reply.send(Err(RuntimeError::TaskAlreadyExists { name }));
            return;
        }

        let (start_tx, start_rx) = watch::channel(false);
        let mut accepted = Vec::with_capacity(items.len());
        for item in items {
            let id = item.id;
            let label = item.label;
            let entry = self.spawn_entry(
                id,
                Arc::clone(&label),
                item.spec,
                None,
                None,
                Some(start_rx.clone()),
            );
            st.tasks.insert(id, entry);
            st.by_label.insert(Arc::clone(&label), id);
            accepted.push((id, label));
        }
        drop(st);

        for (id, label) in accepted {
            self.bus.publish(
                Event::new(EventKind::TaskAdded)
                    .with_task(label)
                    .with_id(id),
            );
        }
        let _ = reply.send(Ok(()));
        start_tx.send_replace(true);
    }

    /// Spawns an actor and registers it under `id`.
    ///
    /// Duplicate task names are rejected.
    ///
    /// Direct `add_and_watch` callers still receive [`RuntimeError::TaskAlreadyExists`](crate::RuntimeError::TaskAlreadyExists) because registration confirmation fails before the waiter is returned.
    async fn spawn_and_register(
        &self,
        id: TaskId,
        spec: TaskSpec,
        done: Option<OutcomeTx>,
        completion: Option<RemovalCompletion>,
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

        let entry = self.spawn_entry(id, Arc::clone(&label), spec, done, completion, None);
        st.tasks.insert(id, entry);
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
        if let Some((_label, handle, completion)) = self.claim_task(id).await {
            handle.cancel.cancel();
            let _ = reply.send(Ok(true));
            self.spawn_join_report(id, handle.join, Some(self.grace), handle.done, completion);
        } else {
            let _ = reply.send(Ok(false));
        }
    }

    /// Resolves one label and claims its current owner under the same state lock.
    ///
    /// A missing label returns `Ok(false)` without a request event.
    /// An owner that is already `Removing` keeps its request event but also returns `Ok(false)`.
    async fn remove_task_by_label(&self, label: Arc<str>, reply: oneshot::Sender<RemoveReply>) {
        let claimed = {
            let mut st = self.state.write().await;
            let Some(id) = st.by_label.get(label.as_ref()).copied() else {
                drop(st);
                let _ = reply.send(Ok(false));
                return;
            };

            self.bus.publish(
                Event::new(EventKind::TaskRemoveRequested)
                    .with_task(Arc::clone(&label))
                    .with_id(id),
            );
            Self::claim_registered(&mut st, &self.pending_joins, id)
                .map(|(_entry_label, handle, completion)| (id, handle, completion))
        };

        if let Some((id, handle, completion)) = claimed {
            handle.cancel.cancel();
            let _ = reply.send(Ok(true));
            self.spawn_join_report(id, handle.join, Some(self.grace), handle.done, completion);
        } else {
            let _ = reply.send(Ok(false));
        }
    }

    /// Claims or joins cancellation by identity and returns a shared terminal decision.
    async fn cancel_task(&self, id: TaskId, reply: oneshot::Sender<CancelReply>) {
        let action = {
            let mut st = self.state.write().await;
            if !st.tasks.contains_key(&id) {
                None
            } else {
                self.bus.publish(
                    Event::new(EventKind::TaskRemoveRequested)
                        .with_id(id)
                        .with_reason("manual_cancel"),
                );
                Self::cancel_action(&mut st, &self.pending_joins, id)
            }
        };
        self.resolve_cancel_action(action, reply);
    }

    /// Resolves a label and claims or joins cancellation under the same state lock.
    async fn cancel_task_by_label(&self, label: Arc<str>, reply: oneshot::Sender<CancelReply>) {
        let action = {
            let mut st = self.state.write().await;
            let Some(id) = st.by_label.get(label.as_ref()).copied() else {
                drop(st);
                let _ = reply.send(Ok(None));
                return;
            };

            self.bus.publish(
                Event::new(EventKind::TaskRemoveRequested)
                    .with_task(label)
                    .with_id(id)
                    .with_reason("manual_cancel"),
            );
            Self::cancel_action(&mut st, &self.pending_joins, id)
        };
        self.resolve_cancel_action(action, reply);
    }

    /// Selects one cancel action while registry state is locked.
    fn cancel_action(
        st: &mut Inner,
        pending_joins: &PendingJoins,
        id: TaskId,
    ) -> Option<CancelAction> {
        let existing_completion = {
            let entry = st.tasks.get(&id)?;
            match &entry.state {
                EntryState::Registered(_) => None,
                EntryState::Removing { completion } => Some(completion.clone()),
            }
        };
        if let Some(completion) = existing_completion {
            return Some(CancelAction {
                decision: CancelDecision {
                    id,
                    claimed: false,
                    completion,
                },
                handle: None,
            });
        }

        let (_label, handle, completion) = Self::claim_registered(st, pending_joins, id)
            .expect("a registered entry must be claimable while state is locked");
        Some(CancelAction {
            decision: CancelDecision {
                id,
                claimed: true,
                completion,
            },
            handle: Some(handle),
        })
    }

    /// Sends one cancel decision and starts the join owner when this command claimed it.
    fn resolve_cancel_action(
        &self,
        action: Option<CancelAction>,
        reply: oneshot::Sender<CancelReply>,
    ) {
        let Some(CancelAction { decision, handle }) = action else {
            let _ = reply.send(Ok(None));
            return;
        };

        if let Some(handle) = handle {
            handle.cancel.cancel();
            let completion = decision.completion.clone();
            let id = decision.id;
            let _ = reply.send(Ok(Some(decision)));
            self.spawn_join_report(id, handle.join, Some(self.grace), handle.done, completion);
        } else {
            let _ = reply.send(Ok(Some(decision)));
        }
    }

    /// Cleans up a finished actor by identity.
    ///
    /// Called after the actor's reliable completion signal is received.
    /// Duplicate or stale completion signals are no-ops.
    async fn cleanup_task(&self, id: TaskId) {
        if let Some((_label, handle, completion)) = self.claim_task(id).await {
            self.spawn_join_report(id, handle.join, Some(self.grace), handle.done, completion);
        }
    }

    /// Changes one task from `Registered` to `Removing`.
    ///
    /// The winning caller gets the only actor handle.
    /// Identity and label indexes stay in the registry until that caller finishes the join.
    async fn claim_task(&self, id: TaskId) -> Option<(Arc<str>, Handle, RemovalCompletion)> {
        let mut st = self.state.write().await;
        Self::claim_registered(&mut st, &self.pending_joins, id)
    }

    /// Locked implementation of the `Registered` to `Removing` transition.
    fn claim_registered(
        st: &mut Inner,
        pending_joins: &PendingJoins,
        id: TaskId,
    ) -> Option<(Arc<str>, Handle, RemovalCompletion)> {
        let entry = st.tasks.get_mut(&id)?;
        if matches!(&entry.state, EntryState::Removing { .. }) {
            return None;
        }

        let completion = match &entry.state {
            EntryState::Registered(handle) => handle.completion.clone(),
            EntryState::Removing { .. } => unreachable!("a removing entry was checked above"),
        };
        let EntryState::Registered(handle) = std::mem::replace(
            &mut entry.state,
            EntryState::Removing {
                completion: completion.clone(),
            },
        ) else {
            unreachable!("a removing entry was checked above")
        };
        let label = Arc::clone(&entry.label);
        pending_joins.inc(id);
        pending_joins.label(id, Arc::clone(&label));
        Some((label, handle, completion))
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
        join: JoinHandle<ActorExitReason>,
        force_after: Option<Duration>,
        done: Option<OutcomeTx>,
        removal_completion: RemovalCompletion,
    ) {
        let bus = self.bus.clone();
        let state = Arc::clone(&self.state);
        let empty_notify = Arc::clone(&self.empty_notify);
        let pending = Arc::clone(&self.pending_joins);
        tokio::spawn(async move {
            let mut join = join;
            let completion = match force_after {
                Some(grace) => match tokio::time::timeout(grace, &mut join).await {
                    Ok(res) => JoinCompletion::Joined(res),
                    Err(_) => {
                        join.abort();
                        let _ = join.await;
                        JoinCompletion::ForceAborted
                    }
                },
                None => JoinCompletion::Joined(join.await),
            };

            Self::finish_removal(
                &state,
                &empty_notify,
                &pending,
                &bus,
                RemovalReport {
                    id,
                    outcome: done,
                    join: completion,
                    completion: removal_completion,
                },
            )
            .await;
        });
    }

    /// Commits terminal cleanup for one `Removing` entry.
    ///
    /// State removal, outcome delivery, terminal events, and pending-join cleanup finish before an empty-registry waiter can continue.
    async fn finish_removal(
        state: &RwLock<Inner>,
        empty_notify: &Notify,
        pending_joins: &PendingJoins,
        bus: &Bus,
        report: RemovalReport,
    ) {
        let RemovalReport {
            id,
            outcome,
            join,
            completion: removal_completion,
        } = report;
        let mut st = state.write().await;
        let is_removing = st
            .tasks
            .get(&id)
            .is_some_and(|entry| matches!(&entry.state, EntryState::Removing { .. }));
        if !is_removing {
            drop(st);
            pending_joins.dec(id);
            removal_completion.complete();
            return;
        }

        let entry = st
            .tasks
            .remove(&id)
            .expect("the removing entry was checked above");
        let EntryState::Removing {
            completion: state_completion,
        } = entry.state
        else {
            unreachable!("the removing entry was checked above")
        };
        if st.by_label.get(entry.label.as_ref()) == Some(&id) {
            st.by_label.remove(entry.label.as_ref());
        }

        match join {
            JoinCompletion::Joined(res) => {
                Self::report_join(bus, id, &entry.label, res, outcome);
            }
            JoinCompletion::ForceAborted => {
                if let Some(done) = outcome {
                    let _ = done.send(TaskOutcome::ForceAborted);
                }
                bus.publish(
                    Event::new(EventKind::TaskRemoved)
                        .with_task(Arc::clone(&entry.label))
                        .with_id(id)
                        .with_reason("force_terminated_after_grace"),
                );
            }
        }
        pending_joins.dec(id);
        let is_empty = st.tasks.is_empty();
        drop(st);

        // Wake completion waiters only after id and label membership is removed and the
        // registry state lock is released. A controller may safely admit the next task now.
        state_completion.complete();
        removal_completion.complete();
        if is_empty {
            empty_notify.notify_waiters();
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
        let (_tx, rx) = mpsc::channel(64);
        Registry::new(
            bus,
            token,
            None,
            Duration::from_secs(5),
            TaskDefaults::default(),
            rx,
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn terminal_cleanup_wakes_all_empty_waiters() {
        use tokio::sync::Barrier;

        let registry = registry();
        let id = TaskId::next();
        let label: Arc<str> = Arc::from("empty-waiters");
        let completion = RemovalCompletion::new();
        let mut state = registry.state.write().await;
        state.by_label.insert(Arc::clone(&label), id);
        state.tasks.insert(
            id,
            Entry {
                label: Arc::clone(&label),
                state: EntryState::Removing {
                    completion: completion.clone(),
                },
            },
        );
        registry.pending_joins.inc(id);
        registry.pending_joins.label(id, label);

        let ready = Arc::new(Barrier::new(3));
        let first_registry = Arc::clone(&registry);
        let first_ready = Arc::clone(&ready);
        let first = tokio::spawn(async move {
            first_ready.wait().await;
            first_registry.wait_until_empty().await;
        });
        let second_registry = Arc::clone(&registry);
        let second_ready = Arc::clone(&ready);
        let second = tokio::spawn(async move {
            second_ready.wait().await;
            second_registry.wait_until_empty().await;
        });

        ready.wait().await;
        tokio::task::yield_now().await;
        drop(state);

        let state_barrier = registry.state.write().await;
        drop(state_barrier);
        assert!(!first.is_finished());
        assert!(!second.is_finished());

        Registry::finish_removal(
            &registry.state,
            &registry.empty_notify,
            &registry.pending_joins,
            &registry.bus,
            RemovalReport {
                id,
                outcome: None,
                join: JoinCompletion::Joined(Ok(ActorExitReason::Completed)),
                completion,
            },
        )
        .await;

        tokio::time::timeout(Duration::from_secs(1), first)
            .await
            .expect("the first empty waiter must wake")
            .expect("the first empty waiter must not panic");
        tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .expect("the second empty waiter must wake")
            .expect("the second empty waiter must not panic");
        assert!(registry.is_empty().await);
        assert_eq!(registry.id_for_label("empty-waiters").await, None);
        assert!(registry.pending_joins.is_empty());
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
        let registry = Registry::new(
            bus.clone(),
            token.clone(),
            None,
            grace,
            TaskDefaults::default(),
            rx,
        );
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
            completion: None,
            reply,
        })
        .expect("registry command channel must stay open");
        reply_rx
    }

    fn batch_item(id: TaskId, spec: TaskSpec) -> AddBatchItem {
        AddBatchItem {
            id,
            label: Arc::from(spec.task().name()),
            spec,
        }
    }

    fn send_batch(tx: &mpsc::Sender<RegistryCommand>, items: Vec<AddBatchItem>) -> AddReplyRx {
        let (reply, reply_rx) = oneshot::channel();
        tx.try_send(RegistryCommand::AddBatch { items, reply })
            .expect("registry command channel must stay open");
        reply_rx
    }

    fn send_remove(tx: &mpsc::Sender<RegistryCommand>, id: TaskId) -> RemoveReplyRx {
        let (reply, reply_rx) = oneshot::channel();
        tx.try_send(RegistryCommand::Remove { id, reply })
            .expect("registry command channel must stay open");
        reply_rx
    }

    fn send_cancel(tx: &mpsc::Sender<RegistryCommand>, id: TaskId) -> CancelReplyRx {
        let (reply, reply_rx) = oneshot::channel();
        tx.try_send(RegistryCommand::Cancel { id, reply })
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
    async fn batch_reply_commits_every_task_as_one_registry_decision() {
        use crate::{TaskContext, TaskFn, TaskRef};

        let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(1));
        let mut events = bus.subscribe();
        let mut expected = Vec::new();
        let mut items = Vec::new();
        for label in ["batch-a", "batch-b", "batch-c"] {
            let id = TaskId::next();
            let task: TaskRef = TaskFn::arc(label, |ctx: TaskContext| async move {
                ctx.cancelled().await;
                Ok(())
            });
            expected.push((id, Arc::from(label)));
            items.push(batch_item(id, TaskSpec::restartable(task)));
        }
        expected.sort_by_key(|(id, _)| *id);

        let result = receive_reply(send_batch(&tx, items), "batch add reply").await;
        assert!(result.is_ok(), "unique batch must be accepted: {result:?}");
        assert_eq!(registry.list().await, expected);

        let added: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|event| event.kind == EventKind::TaskAdded)
            .collect();
        assert_eq!(added.len(), 3);

        stop_registry(&registry, &token).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_batch_reply_still_starts_after_all_added_events() {
        use crate::{TaskContext, TaskFn, TaskRef};

        let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(1));
        let mut events = bus.subscribe();
        let (body_tx, mut body_rx) = mpsc::unbounded_channel();
        let mut items = Vec::new();
        for label in ["dropped-batch-a", "dropped-batch-b"] {
            let id = TaskId::next();
            let body_tx = body_tx.clone();
            let task: TaskRef = TaskFn::arc(label, move |_ctx: TaskContext| {
                let _ = body_tx.send(id);
                async { Ok(()) }
            });
            items.push(batch_item(id, TaskSpec::once(task)));
        }
        drop(body_tx);

        let reply = send_batch(&tx, items);
        drop(reply);
        let first = receive_completion(&mut body_rx, "first batch body").await;
        let second = receive_completion(&mut body_rx, "second batch body").await;
        assert_ne!(first, second);
        tokio::time::timeout(Duration::from_secs(2), registry.wait_until_empty())
            .await
            .expect("both one-shot batch tasks must finish");

        let observed: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
        let added: Vec<_> = observed
            .iter()
            .filter(|event| event.kind == EventKind::TaskAdded)
            .collect();
        let starting: Vec<_> = observed
            .iter()
            .filter(|event| event.kind == EventKind::TaskStarting)
            .collect();
        assert_eq!(added.len(), 2);
        assert_eq!(starting.len(), 2);
        let last_added = added.iter().map(|event| event.seq).max().unwrap();
        let first_starting = starting.iter().map(|event| event.seq).min().unwrap();
        assert!(
            last_added < first_starting,
            "the batch start gate must keep bodies behind all TaskAdded events"
        );

        stop_registry(&registry, &token).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplicate_inside_batch_rejects_every_item_without_starting_bodies() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::{TaskContext, TaskFn, TaskRef};

        let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(1));
        let mut events = bus.subscribe();
        let runs = Arc::new(AtomicUsize::new(0));
        let mut items = Vec::new();
        let mut ids = Vec::new();
        for label in ["unique", "duplicate", "duplicate"] {
            let runs = Arc::clone(&runs);
            let task: TaskRef = TaskFn::arc(label, move |_ctx: TaskContext| {
                runs.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            });
            let id = TaskId::next();
            ids.push(id);
            items.push(batch_item(id, TaskSpec::once(task)));
        }

        let result = receive_reply(send_batch(&tx, items), "duplicate batch reply").await;
        assert!(
            matches!(
                result,
                Err(RuntimeError::TaskAlreadyExists { ref name }) if name.as_ref() == "duplicate"
            ),
            "the first conflicting input label must reject the batch: {result:?}"
        );
        assert!(registry.list().await.is_empty());
        assert_eq!(runs.load(Ordering::SeqCst), 0);

        let observed: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
        assert_eq!(
            observed
                .iter()
                .filter(|event| event.kind == EventKind::TaskAdded)
                .count(),
            0
        );
        let failed: Vec<_> = observed
            .into_iter()
            .filter(|event| event.kind == EventKind::TaskAddFailed)
            .collect();
        assert_eq!(failed.len(), 3);
        assert_eq!(failed[0].id, Some(ids[0]));
        assert_eq!(failed[0].reason.as_deref(), Some(reasons::BATCH_REJECTED));
        assert_eq!(failed[1].id, Some(ids[1]));
        assert_eq!(failed[1].reason.as_deref(), Some(reasons::BATCH_REJECTED));
        assert_eq!(failed[2].id, Some(ids[2]));
        assert_eq!(failed[2].reason.as_deref(), Some(reasons::ALREADY_EXISTS));

        stop_registry(&registry, &token).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn batch_conflict_with_registered_or_removing_label_starts_no_new_body() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::{TaskContext, TaskError, TaskFn, TaskRef};

        let (registry, _bus, token, tx) = started_registry(64, Duration::from_secs(1));
        let started = Arc::new(Notify::new());
        let cancellation_seen = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let started_by_task = Arc::clone(&started);
        let seen_by_task = Arc::clone(&cancellation_seen);
        let release_by_task = Arc::clone(&release);
        let existing: TaskRef = TaskFn::arc("reserved-batch-name", move |ctx: TaskContext| {
            let started = Arc::clone(&started_by_task);
            let cancellation_seen = Arc::clone(&seen_by_task);
            let release = Arc::clone(&release_by_task);
            async move {
                started.notify_one();
                ctx.cancelled().await;
                cancellation_seen.notify_one();
                release.notified().await;
                Err(TaskError::Canceled)
            }
        });
        let existing_id = TaskId::next();
        assert!(
            receive_reply(
                send_add(&tx, existing_id, TaskSpec::restartable(existing), None,),
                "existing add reply",
            )
            .await
            .is_ok()
        );
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .expect("the existing task body must start before removal");

        let candidate_runs = Arc::new(AtomicUsize::new(0));
        let make_candidate = |label: &'static str| {
            let runs = Arc::clone(&candidate_runs);
            let task: TaskRef = TaskFn::arc(label, move |_ctx: TaskContext| {
                runs.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            });
            batch_item(TaskId::next(), TaskSpec::once(task))
        };

        let registered_result = receive_reply(
            send_batch(
                &tx,
                vec![
                    make_candidate("registered-peer"),
                    make_candidate("reserved-batch-name"),
                ],
            ),
            "registered conflict batch",
        )
        .await;
        assert!(matches!(
            registered_result,
            Err(RuntimeError::TaskAlreadyExists { name })
                if name.as_ref() == "reserved-batch-name"
        ));
        assert_eq!(candidate_runs.load(Ordering::SeqCst), 0);
        assert_eq!(
            registry.list().await,
            vec![(existing_id, Arc::from("reserved-batch-name"))]
        );

        assert!(matches!(
            receive_reply(send_remove(&tx, existing_id), "existing remove reply").await,
            Ok(true)
        ));
        tokio::time::timeout(Duration::from_secs(2), cancellation_seen.notified())
            .await
            .expect("the existing task must enter Removing");

        let removing_result = receive_reply(
            send_batch(
                &tx,
                vec![
                    make_candidate("removing-peer"),
                    make_candidate("reserved-batch-name"),
                ],
            ),
            "removing conflict batch",
        )
        .await;
        assert!(matches!(
            removing_result,
            Err(RuntimeError::TaskAlreadyExists { name })
                if name.as_ref() == "reserved-batch-name"
        ));
        assert_eq!(candidate_runs.load(Ordering::SeqCst), 0);
        assert_eq!(
            registry.list().await,
            vec![(existing_id, Arc::from("reserved-batch-name"))]
        );

        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), registry.wait_until_empty())
            .await
            .expect("the existing removing task must finish");
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
        assert!(
            registry.contains(id).await,
            "a removing task must keep its registry identity"
        );
        assert_eq!(
            registry.list().await,
            vec![(id, Arc::from("remove-once"))],
            "a removing task must stay visible in registry listings"
        );
        assert_eq!(registry.id_for_label("remove-once").await, Some(id));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), registry.wait_until_empty())
                .await
                .is_err(),
            "the registry cannot become empty before the actor join"
        );
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

        let joined_cancel = receive_reply(send_cancel(&tx, id), "joined cancel reply")
            .await
            .expect("the cancel command must succeed")
            .expect("the removing task must expose its completion");
        assert!(
            !joined_cancel.claimed,
            "cancel must join an existing Remove instead of claiming again"
        );
        assert!(
            !joined_cancel.is_complete(),
            "joining cancellation cannot complete before the actor join"
        );

        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), joined_cancel.wait())
            .await
            .expect("joined cancellation must finish with the Remove owner");
        tokio::time::timeout(
            Duration::from_secs(2),
            registry.pending_joins.wait_drained(),
        )
        .await
        .expect("the released task must finish its join");
        assert!(!registry.contains(id).await);
        assert_eq!(registry.id_for_label("remove-once").await, None);
        stop_registry(&registry, &token).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_cancel_commands_share_one_terminal_completion() {
        use crate::{TaskContext, TaskError, TaskFn, TaskRef};

        let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(5));
        let mut events = bus.subscribe();
        let cancellation_seen = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let seen_by_task = Arc::clone(&cancellation_seen);
        let task_release = Arc::clone(&release);
        let task: TaskRef = TaskFn::arc("shared-cancel", move |ctx: TaskContext| {
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
                "shared cancel add reply",
            )
            .await
            .is_ok()
        );
        while events.try_recv().is_ok() {}

        const CALLERS: usize = 8;
        let replies: Vec<_> = (0..CALLERS).map(|_| send_cancel(&tx, id)).collect();
        let mut decisions = Vec::with_capacity(CALLERS);
        for reply in replies {
            decisions.push(
                receive_reply(reply, "concurrent cancel reply")
                    .await
                    .expect("cancel command must succeed")
                    .expect("the task must still be removing"),
            );
        }
        tokio::time::timeout(Duration::from_secs(2), cancellation_seen.notified())
            .await
            .expect("the task must observe one cancellation");

        assert_eq!(
            decisions.iter().filter(|decision| decision.claimed).count(),
            1,
            "exactly one cancellation command may claim the task"
        );
        assert!(decisions.iter().all(|decision| !decision.is_complete()));
        assert!(registry.contains(id).await);
        assert!(
            std::iter::from_fn(|| events.try_recv().ok())
                .all(|event| event.id != Some(id) || event.kind != EventKind::TaskRemoved),
            "terminal cleanup cannot happen before the task is released"
        );

        release.notify_one();
        for decision in &decisions {
            tokio::time::timeout(Duration::from_secs(2), decision.wait())
                .await
                .expect("all cancel callers must share terminal completion");
        }
        tokio::time::timeout(Duration::from_secs(2), registry.wait_until_empty())
            .await
            .expect("terminal cleanup must remove the task");
        let removed = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|event| event.id == Some(id) && event.kind == EventKind::TaskRemoved)
            .count();
        assert_eq!(
            removed, 1,
            "shared cancellation must publish one terminal event"
        );

        stop_registry(&registry, &token).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn removing_task_keeps_label_reserved_until_terminal_join() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::{TaskContext, TaskError, TaskFn, TaskRef};

        let (registry, _bus, token, tx) = started_registry(64, Duration::from_secs(5));
        let cancellation_seen = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let seen_by_task = Arc::clone(&cancellation_seen);
        let task_release = Arc::clone(&release);
        let first: TaskRef = TaskFn::arc("reserved-name", move |ctx: TaskContext| {
            let seen = Arc::clone(&seen_by_task);
            let release = Arc::clone(&task_release);
            async move {
                ctx.cancelled().await;
                seen.notify_one();
                release.notified().await;
                Err(TaskError::Canceled)
            }
        });
        let first_id = TaskId::next();
        assert!(
            receive_reply(
                send_add(&tx, first_id, TaskSpec::restartable(first), None),
                "reserved-name add reply",
            )
            .await
            .is_ok()
        );
        assert!(matches!(
            receive_reply(send_remove(&tx, first_id), "reserved-name remove reply").await,
            Ok(true)
        ));
        tokio::time::timeout(Duration::from_secs(2), cancellation_seen.notified())
            .await
            .expect("the old task must observe cancellation");

        let duplicate_runs = Arc::new(AtomicUsize::new(0));
        let runs_by_task = Arc::clone(&duplicate_runs);
        let duplicate: TaskRef = TaskFn::arc("reserved-name", move |_ctx: TaskContext| {
            runs_by_task.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        });
        let duplicate_id = TaskId::next();
        let duplicate_reply = receive_reply(
            send_add(&tx, duplicate_id, TaskSpec::once(duplicate), None),
            "removing duplicate add reply",
        )
        .await;
        assert!(
            matches!(
                duplicate_reply,
                Err(RuntimeError::TaskAlreadyExists { name })
                    if name.as_ref() == "reserved-name"
            ),
            "a removing task must keep its label reserved"
        );
        assert_eq!(
            duplicate_runs.load(Ordering::SeqCst),
            0,
            "a rejected replacement body must not run"
        );
        assert_eq!(registry.id_for_label("reserved-name").await, Some(first_id));
        assert_eq!(
            registry.list().await,
            vec![(first_id, Arc::from("reserved-name"))]
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), registry.wait_until_empty())
                .await
                .is_err(),
            "terminal join must control when the registry becomes empty"
        );

        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), registry.wait_until_empty())
            .await
            .expect("terminal join must release the old task identity");
        assert_eq!(registry.id_for_label("reserved-name").await, None);
        assert!(!registry.pending_joins.contains(first_id));

        let replacement: TaskRef = TaskFn::arc("reserved-name", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        let replacement_id = TaskId::next();
        assert!(
            receive_reply(
                send_add(
                    &tx,
                    replacement_id,
                    TaskSpec::restartable(replacement),
                    None,
                ),
                "replacement add reply",
            )
            .await
            .is_ok(),
            "the label must be reusable after terminal cleanup"
        );
        assert_eq!(
            registry.id_for_label("reserved-name").await,
            Some(replacement_id)
        );

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
            Entry {
                label: Arc::clone(&label),
                state: EntryState::Registered(Handle {
                    join,
                    cancel: CancellationToken::new(),
                    done: Some(done),
                    completion: RemovalCompletion::new(),
                }),
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

        let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(5));
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
        let (done, mut done_rx) = oneshot::channel();
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
        assert!(matches!(
            receive_reply(
                send_remove(&tx, TaskId::next()),
                "completion claim barrier reply",
            )
            .await,
            Ok(false)
        ));
        assert!(registry.pending_joins.contains(id));
        assert!(registry.contains(id).await);
        assert_eq!(registry.id_for_label("completion-first").await, Some(id));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), registry.wait_until_empty())
                .await
                .is_err(),
            "completion ownership must retain membership until join"
        );
        assert!(matches!(
            receive_reply(send_remove(&tx, id), "completion-first remove reply").await,
            Ok(false)
        ));
        let joined_cancel = receive_reply(send_cancel(&tx, id), "completion-first cancel reply")
            .await
            .expect("the cancel command must succeed")
            .expect("the completion-owned removal must still exist");
        assert!(
            !joined_cancel.claimed,
            "cancel must join the completion-plane owner"
        );
        assert!(!joined_cancel.is_complete());
        assert!(
            std::iter::from_fn(|| events.try_recv().ok())
                .all(|event| event.id != Some(id) || event.kind != EventKind::TaskRemoved),
            "remove must not report termination while cleanup is still joining"
        );

        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), joined_cancel.wait())
            .await
            .expect("cancel must finish with the completion-plane owner");
        assert!(
            matches!(done_rx.try_recv(), Ok(TaskOutcome::Completed)),
            "watched outcome must be ready before terminal completion is signalled"
        );
        tokio::time::timeout(Duration::from_secs(2), registry.wait_until_empty())
            .await
            .expect("completion-owned join must finish registry cleanup");
        assert!(!registry.pending_joins.contains(id));

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
        let reg = Registry::new(
            bus,
            token.clone(),
            None,
            Duration::from_millis(50),
            TaskDefaults::default(),
            rx,
        );

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
            completion: None,
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
