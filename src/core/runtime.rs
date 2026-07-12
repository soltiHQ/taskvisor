//! # Supervisor runtime core.
//!
//! [`SupervisorCore`] owns the runtime components behind the public [`Supervisor`](super::supervisor::Supervisor) facade.
//!
//! It wires together:
//! - registry command channel,
//! - [`Registry`],
//! - event bus,
//! - subscriber fan-out,
//! - alive-task tracker,
//! - runtime shutdown token.
//!
//! ## Planes
//!
//! ```text
//! Management plane:
//!   SupervisorHandle -> SupervisorCore -> mpsc -> Registry
//!
//! Event plane:
//!   runtime components -> Bus -> subscriber_listener
//!                               -> AliveTracker
//!                               -> SubscriberSet
//!
//! Shutdown plane:
//!   OS signal / handle.shutdown -> drain_with_grace
//!                               -> cancel registry tasks
//!                               -> join listeners
//!                               -> close subscribers
//! ```
//!
//! Add, remove, and cancel commands use the management plane.
//! They are not delivered through the lossy event bus.
//!
//! Events are used for observability, alive snapshots, and subscriber delivery.
//! Event consumers may lag.
//!
//! ## Modes
//!
//! ```text
//! start()
//!   starts subscriber listener and registry listener
//!
//! run(tasks)
//!   starts listeners
//!   adds initial tasks
//!   waits for registration confirmation best-effort
//!   waits for OS shutdown signal or natural completion
//!
//! shutdown()
//!   cancels all tasks
//!   waits up to grace
//!   force-aborts tasks that do not stop
//!   joins internal listeners
//! ```
//!
//! ## Rules
//!
//! - New task admission closes once shutdown begins.
//! - Shutdown waits for a registry fence before it starts task drain.
//! - Commands committed before the admission gate closes are processed before that fence.
//! - Registry membership is keyed by `TaskId`.
//! - `run()` is single-shot. A second call returns `RuntimeError::AlreadyRunning`.
//! - `snapshot` and `is_alive` are best-effort views from the alive tracker.
//! - `start()` is idempotent.

use std::sync::atomic::{AtomicBool, Ordering};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::{broadcast, mpsc, oneshot},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

use crate::core::{
    alive::AliveTracker,
    registry::{
        AddReplyRx, CancelDecision, CancelReplyRx, OutcomeTx, Registry, RegistryCommand,
        RemoveReplyRx,
    },
};
use crate::{
    core::SupervisorConfig,
    error::RuntimeError,
    events::{Bus, Event, EventKind},
    identity::TaskId,
    subscribers::SubscriberSet,
    tasks::TaskSpec,
};

/// Runtime implementation behind the public [`Supervisor`](super::supervisor::Supervisor).
///
/// This type is controller-agnostic.
/// The public facade may compose it with an optional controller, but the core itself only manages
/// registry commands, events, subscribers, alive tracking, and shutdown.
pub(crate) struct SupervisorCore {
    cfg: SupervisorConfig,
    pub(super) bus: Bus,
    subs: Arc<SubscriberSet>,
    alive: Arc<AliveTracker>,
    registry: Arc<Registry>,
    runtime_token: CancellationToken,
    started: AtomicBool,
    running: AtomicBool,
    shutting_down: AtomicBool,
    admission_gate: std::sync::Mutex<()>,
    cmd_tx: mpsc::Sender<RegistryCommand>,
    subscriber_handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for SupervisorCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupervisorCore")
            .field("cfg", &self.cfg)
            .field("started", &self.started.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl SupervisorCore {
    /// Creates a ready-to-share runtime core.
    ///
    /// Used by the builder after all runtime components have been wired.
    pub(crate) fn new_internal(
        cfg: SupervisorConfig,
        bus: Bus,
        subs: Arc<SubscriberSet>,
        alive: Arc<AliveTracker>,
        registry: Arc<Registry>,
        runtime_token: CancellationToken,
        cmd_tx: mpsc::Sender<RegistryCommand>,
    ) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            bus,
            subs,
            alive,
            registry,
            runtime_token,
            started: AtomicBool::new(false),
            running: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            admission_gate: std::sync::Mutex::new(()),
            cmd_tx,
            subscriber_handle: std::sync::Mutex::new(None),
        })
    }

    /// Returns true once shutdown has started and management admission is closed.
    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    /// Marks the runtime as shutting down.
    ///
    /// The gate lock waits for every command that already passed its final
    /// admission check to become visible in the registry queue.
    fn mark_shutting_down(&self) {
        let _gate = self
            .admission_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.shutting_down.store(true, Ordering::Release);
    }

    /// Holds the admission gate across a command's final check and queue commit.
    fn command_admission(&self) -> Option<std::sync::MutexGuard<'_, ()>> {
        let gate = self
            .admission_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.is_shutting_down() {
            None
        } else {
            Some(gate)
        }
    }

    /// Closes command admission and waits until the registry reaches that ordering point.
    ///
    /// Every command committed before the gate closes is ahead of this fence.
    /// Backpressured callers re-check the gate after receiving capacity and are
    /// rejected instead of appearing behind the fence.
    async fn close_admission_and_fence_registry(&self) -> Result<(), RuntimeError> {
        self.mark_shutting_down();
        self.registry.fence().await
    }

    /// Adds a task and waits for the registry registration decision.
    ///
    /// This waits for queue capacity before sending the command.
    pub(crate) async fn add_task(&self, spec: TaskSpec) -> Result<TaskId, RuntimeError> {
        let (id, reply) = self
            .enqueue_add_task_wait(TaskId::next(), spec, None)
            .await
            .map_err(|(error, _done)| error)?;
        Self::await_add_reply(id, reply).await
    }

    /// Tries to add a task without waiting for queue capacity.
    ///
    /// After the command enters the queue, this still waits for the registry registration decision.
    pub(crate) async fn try_add_task(&self, spec: TaskSpec) -> Result<TaskId, RuntimeError> {
        let (id, reply) = self
            .enqueue_add_task(TaskId::next(), spec, None)
            .map_err(|(error, _done)| error)?;
        Self::await_add_reply(id, reply).await
    }

    /// Queues a task add command under a pre-minted identity.
    ///
    /// Used by the controller so a submission keeps the same [`TaskId`] from admission through registry registration.
    ///
    /// Unlike [`add_task`](Self::add_task), this **hands the watcher `done` back** in the error
    /// tuple on failure instead of dropping it, so the controller can resolve the submission's
    /// waiter with `Rejected` rather than leaving it to observe a canceled oneshot.
    #[cfg(feature = "controller")]
    pub(crate) fn add_task_with_id_watched(
        &self,
        id: TaskId,
        spec: TaskSpec,
        done: Option<OutcomeTx>,
    ) -> Result<TaskId, (RuntimeError, Option<OutcomeTx>)> {
        let (id, reply) = self.enqueue_add_task(id, spec, done)?;
        drop(reply);
        Ok(id)
    }

    /// Adds a watched task and waits for the registry registration decision.
    ///
    /// Returns the minted [`TaskId`] and a receiver that resolves to the final [`TaskOutcome`](crate::TaskOutcome)
    /// if the task is registered and later terminates.
    pub(crate) async fn add_task_watched(
        &self,
        spec: TaskSpec,
    ) -> Result<(TaskId, tokio::sync::oneshot::Receiver<crate::TaskOutcome>), RuntimeError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (id, reply) = self
            .enqueue_add_task_wait(TaskId::next(), spec, Some(tx))
            .await
            .map_err(|(error, _done)| error)?;
        let id = Self::await_add_reply(id, reply).await?;
        Ok((id, rx))
    }

    /// Resolves one authoritative registry Add reply.
    async fn await_add_reply(id: TaskId, reply: AddReplyRx) -> Result<TaskId, RuntimeError> {
        match reply.await {
            Ok(Ok(())) => Ok(id),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(RuntimeError::ShuttingDown),
        }
    }

    /// Queues one add command and returns its authoritative registry reply.
    ///
    /// Used by `try_add` and the controller's fail-fast admission path.
    fn enqueue_add_task(
        &self,
        id: TaskId,
        spec: TaskSpec,
        done: Option<OutcomeTx>,
    ) -> Result<(TaskId, AddReplyRx), (RuntimeError, Option<OutcomeTx>)> {
        if self.is_shutting_down() {
            return Err((RuntimeError::ShuttingDown, done));
        }
        let label: Arc<str> = Arc::from(spec.task().name());
        let permit = match self.cmd_tx.try_reserve() {
            Ok(permit) => permit,
            Err(mpsc::error::TrySendError::Full(())) => {
                return Err((RuntimeError::CommandQueueFull, done));
            }
            Err(mpsc::error::TrySendError::Closed(())) => {
                return Err((RuntimeError::ShuttingDown, done));
            }
        };
        let Some(_admission) = self.command_admission() else {
            drop(permit);
            return Err((RuntimeError::ShuttingDown, done));
        };
        Ok(self.commit_add(permit, id, label, spec, done))
    }

    /// Waits for bounded queue capacity, then queues one Add command.
    async fn enqueue_add_task_wait(
        &self,
        id: TaskId,
        spec: TaskSpec,
        done: Option<OutcomeTx>,
    ) -> Result<(TaskId, AddReplyRx), (RuntimeError, Option<OutcomeTx>)> {
        if self.is_shutting_down() {
            return Err((RuntimeError::ShuttingDown, done));
        }
        let label: Arc<str> = Arc::from(spec.task().name());
        let permit = match self.cmd_tx.reserve().await {
            Ok(permit) => permit,
            Err(_) => return Err((RuntimeError::ShuttingDown, done)),
        };
        let Some(_admission) = self.command_admission() else {
            drop(permit);
            return Err((RuntimeError::ShuttingDown, done));
        };
        Ok(self.commit_add(permit, id, label, spec, done))
    }

    /// Publishes the request event and makes an already-reserved Add visible.
    ///
    /// Reserving capacity before this call keeps rejected commands silent while
    /// preserving `TaskAddRequested` before the registry result event.
    fn commit_add(
        &self,
        permit: mpsc::Permit<'_, RegistryCommand>,
        id: TaskId,
        label: Arc<str>,
        spec: TaskSpec,
        done: Option<OutcomeTx>,
    ) -> (TaskId, AddReplyRx) {
        let (reply, reply_rx) = oneshot::channel();
        self.bus.publish(
            Event::new(EventKind::TaskAddRequested)
                .with_task(label)
                .with_id(id),
        );
        permit.send(RegistryCommand::Add {
            id,
            spec,
            outcome: done,
            reply,
        });
        (id, reply_rx)
    }

    /// Removes a task after queue capacity and the registry claim decision.
    pub(crate) async fn remove(&self, id: TaskId) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_remove_wait(id, None).await?;
        Self::await_remove_reply(reply).await
    }

    /// Tries to remove a task without waiting for command queue capacity.
    pub(crate) async fn try_remove(&self, id: TaskId) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_remove(id, None)?;
        Self::await_remove_reply(reply).await
    }

    /// Removes the task that owns `label` at the registry ordering point.
    pub(crate) async fn remove_by_label(&self, label: Arc<str>) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_remove_by_label_wait(label).await?;
        Self::await_remove_reply(reply).await
    }

    /// Resolves one authoritative registry Remove reply.
    async fn await_remove_reply(reply: RemoveReplyRx) -> Result<bool, RuntimeError> {
        match reply.await {
            Ok(result) => result,
            Err(_) => Err(RuntimeError::ShuttingDown),
        }
    }

    /// Publishes one remove request, queues its command, and returns the authoritative reply.
    fn enqueue_remove(
        &self,
        id: TaskId,
        reason: Option<&'static str>,
    ) -> Result<RemoveReplyRx, RuntimeError> {
        if self.is_shutting_down() {
            return Err(RuntimeError::ShuttingDown);
        }
        let permit = self.cmd_tx.try_reserve().map_err(|error| match error {
            mpsc::error::TrySendError::Full(()) => RuntimeError::CommandQueueFull,
            mpsc::error::TrySendError::Closed(()) => RuntimeError::ShuttingDown,
        })?;
        let Some(_admission) = self.command_admission() else {
            drop(permit);
            return Err(RuntimeError::ShuttingDown);
        };
        Ok(self.commit_remove(permit, id, reason))
    }

    /// Waits for bounded queue capacity, then queues one Remove command.
    async fn enqueue_remove_wait(
        &self,
        id: TaskId,
        reason: Option<&'static str>,
    ) -> Result<RemoveReplyRx, RuntimeError> {
        if self.is_shutting_down() {
            return Err(RuntimeError::ShuttingDown);
        }
        let permit = self
            .cmd_tx
            .reserve()
            .await
            .map_err(|_| RuntimeError::ShuttingDown)?;
        let Some(_admission) = self.command_admission() else {
            drop(permit);
            return Err(RuntimeError::ShuttingDown);
        };
        Ok(self.commit_remove(permit, id, reason))
    }

    /// Waits for queue capacity, then sends one atomic label Remove command.
    async fn enqueue_remove_by_label_wait(
        &self,
        label: Arc<str>,
    ) -> Result<RemoveReplyRx, RuntimeError> {
        if self.is_shutting_down() {
            return Err(RuntimeError::ShuttingDown);
        }
        let permit = self
            .cmd_tx
            .reserve()
            .await
            .map_err(|_| RuntimeError::ShuttingDown)?;
        let Some(_admission) = self.command_admission() else {
            drop(permit);
            return Err(RuntimeError::ShuttingDown);
        };

        let (reply, reply_rx) = oneshot::channel();
        permit.send(RegistryCommand::RemoveByLabel { label, reply });
        Ok(reply_rx)
    }

    /// Publishes one identity request and makes its reserved command visible.
    fn commit_remove(
        &self,
        permit: mpsc::Permit<'_, RegistryCommand>,
        id: TaskId,
        reason: Option<&'static str>,
    ) -> RemoveReplyRx {
        let (reply, reply_rx) = oneshot::channel();
        let mut event = Event::new(EventKind::TaskRemoveRequested).with_id(id);
        if let Some(reason) = reason {
            event = event.with_reason(reason);
        }
        self.bus.publish(event);
        permit.send(RegistryCommand::Remove { id, reply });
        reply_rx
    }

    /// Queues one fail-fast Cancel command by identity.
    fn enqueue_cancel(&self, id: TaskId) -> Result<CancelReplyRx, RuntimeError> {
        if self.is_shutting_down() {
            return Err(RuntimeError::ShuttingDown);
        }
        let permit = self.cmd_tx.try_reserve().map_err(|error| match error {
            mpsc::error::TrySendError::Full(()) => RuntimeError::CommandQueueFull,
            mpsc::error::TrySendError::Closed(()) => RuntimeError::ShuttingDown,
        })?;
        let Some(_admission) = self.command_admission() else {
            drop(permit);
            return Err(RuntimeError::ShuttingDown);
        };

        let (reply, reply_rx) = oneshot::channel();
        permit.send(RegistryCommand::Cancel { id, reply });
        Ok(reply_rx)
    }

    /// Queues one fail-fast atomic Cancel command by label.
    fn enqueue_cancel_by_label(&self, label: Arc<str>) -> Result<CancelReplyRx, RuntimeError> {
        if self.is_shutting_down() {
            return Err(RuntimeError::ShuttingDown);
        }
        let permit = self.cmd_tx.try_reserve().map_err(|error| match error {
            mpsc::error::TrySendError::Full(()) => RuntimeError::CommandQueueFull,
            mpsc::error::TrySendError::Closed(()) => RuntimeError::ShuttingDown,
        })?;
        let Some(_admission) = self.command_admission() else {
            drop(permit);
            return Err(RuntimeError::ShuttingDown);
        };

        let (reply, reply_rx) = oneshot::channel();
        permit.send(RegistryCommand::CancelByLabel { label, reply });
        Ok(reply_rx)
    }

    /// Returns registered tasks as `(id, label)` pairs from the registry.
    pub(crate) async fn list_tasks(&self) -> Vec<(TaskId, Arc<str>)> {
        self.registry.list().await
    }

    /// Returns true if `id` is currently registered.
    #[cfg(any(test, feature = "controller"))]
    pub(crate) async fn contains_id(&self, id: TaskId) -> bool {
        self.registry.contains(id).await
    }

    /// Resolves a label to the identity currently holding it (if any).
    #[cfg(test)]
    pub(crate) async fn id_for_label(&self, name: &str) -> Option<TaskId> {
        self.registry.id_for_label(name).await
    }

    /// Starts runtime listeners without blocking.
    ///
    /// This starts:
    /// - the subscriber listener,
    /// - the registry listener.
    ///
    /// Safe to call more than once. Later calls are no-ops.
    pub(crate) fn start(&self) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        self.subscriber_listener();
        self.registry.clone().spawn_listener();
    }

    /// Runs a static task set until OS shutdown signal or natural completion.
    ///
    /// This starts the runtime listeners, queues the initial tasks, waits for their
    /// registration confirmations best-effort, then drives shutdown/natural
    /// completion.
    ///
    /// Single-shot: a second or concurrent call returns [`RuntimeError::AlreadyRunning`].
    pub(crate) async fn run(&self, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
        if self.running.swap(true, Ordering::AcqRel) {
            return Err(RuntimeError::AlreadyRunning);
        }
        self.start();

        if !tasks.is_empty() {
            let mut rx = self.bus.subscribe();
            let mut pending_ids = Vec::with_capacity(tasks.len());
            for spec in tasks {
                let (id, reply) = self
                    .enqueue_add_task_wait(TaskId::next(), spec, None)
                    .await
                    .map_err(|(error, _done)| error)?;
                drop(reply);
                pending_ids.push(id);
            }
            self.wait_tasks_registered(&mut rx, &pending_ids).await;
        }
        self.drive_shutdown().await
    }

    /// Best-effort wait for initial task registration events.
    ///
    /// Waits for `TaskAdded` or `TaskAddFailed` for every initial task id.
    /// If the bus receiver lags, falls back to registry membership.
    ///
    /// This wait has a fixed backstop and currently does not return an error when the backstop expires.
    async fn wait_tasks_registered(
        &self,
        rx: &mut broadcast::Receiver<Arc<Event>>,
        ids: &[TaskId],
    ) {
        let mut pending: Vec<TaskId> = ids.to_vec();

        let confirm = async {
            while !pending.is_empty() {
                match rx.recv().await {
                    Ok(ev)
                        if matches!(ev.kind, EventKind::TaskAdded | EventKind::TaskAddFailed) =>
                    {
                        if let Some(id) = ev.id {
                            pending.retain(|p| *p != id);
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let mut still = Vec::new();
                        for id in std::mem::take(&mut pending) {
                            if !self.registry.contains(id).await {
                                still.push(id);
                            }
                        }
                        pending = still;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        };
        const CONFIRM_BACKSTOP: Duration = Duration::from_secs(5);
        let _ = timeout(CONFIRM_BACKSTOP, confirm).await;
    }

    /// Initiates explicit graceful shutdown.
    ///
    /// Publishes `ShutdownRequested`, drains tasks with grace, cancels the runtime token, joins internal listeners,
    /// and closes subscriber workers within their configured timeout.
    pub(crate) async fn shutdown(&self) -> Result<(), RuntimeError> {
        self.bus.publish(Event::new(EventKind::ShutdownRequested));
        let res = self.drain_with_grace().await;
        self.runtime_token.cancel();
        self.registry.join_listener().await;
        self.join_subscriber_listener().await;
        self.subs.close().await;
        res
    }

    /// Returns a best-effort sorted list of task names currently marked alive.
    pub(crate) async fn snapshot(&self) -> Vec<Arc<str>> {
        self.alive.snapshot().await
    }

    /// Returns true if any task with this name is currently marked alive.
    ///
    /// This is a best-effort label query from the alive tracker.
    pub(crate) async fn is_alive(&self, name: &str) -> bool {
        self.alive.is_alive(name).await
    }

    /// Cancels a task by identity and waits for registry terminal completion.
    pub(crate) async fn cancel(&self, id: TaskId) -> Result<bool, RuntimeError> {
        let decision = Self::await_cancel_reply(self.enqueue_cancel(id)?).await?;
        Self::wait_cancel_decision(decision, None).await
    }

    /// Cancels a task with an explicit confirmation window.
    ///
    /// The registry decision is not part of `wait_for`. The timeout only bounds
    /// this caller's wait for shared terminal completion and does not stop removal.
    pub(crate) async fn cancel_with_timeout(
        &self,
        id: TaskId,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        let decision = Self::await_cancel_reply(self.enqueue_cancel(id)?).await?;
        Self::wait_cancel_decision(decision, Some(wait_for)).await
    }

    /// Cancels the task that owns `label` at the registry ordering point.
    pub(crate) async fn cancel_by_label(&self, label: Arc<str>) -> Result<bool, RuntimeError> {
        let decision = Self::await_cancel_reply(self.enqueue_cancel_by_label(label)?).await?;
        Self::wait_cancel_decision(decision, None).await
    }

    /// Resolves one authoritative registry cancellation decision.
    async fn await_cancel_reply(
        reply: CancelReplyRx,
    ) -> Result<Option<CancelDecision>, RuntimeError> {
        match reply.await {
            Ok(result) => result,
            Err(_) => Err(RuntimeError::ShuttingDown),
        }
    }

    /// Waits for one shared terminal completion and preserves its claim result.
    async fn wait_cancel_decision(
        decision: Option<CancelDecision>,
        wait_for: Option<Duration>,
    ) -> Result<bool, RuntimeError> {
        let Some(decision) = decision else {
            return Ok(false);
        };
        let id = decision.id;
        let claimed = decision.claimed;

        if let Some(wait_for) = wait_for {
            if timeout(wait_for, decision.wait()).await.is_err() && !decision.is_complete() {
                return Err(RuntimeError::TaskRemoveTimeout {
                    id,
                    timeout: wait_for,
                });
            }
        } else {
            decision.wait().await;
        }

        Ok(claimed)
    }

    /// Applies one event to alive tracking and subscriber fan-out.
    async fn distribute(alive: &AliveTracker, set: &SubscriberSet, ev: Arc<Event>) {
        alive.update(&ev).await;
        set.emit_arc(ev);
    }

    /// Drains retained events from a bus receiver.
    ///
    /// Used when the subscriber listener is shutting down.
    /// Broadcast lag gaps are skipped so the retained tail can still be delivered to alive tracking and subscribers.
    async fn drain_pending(
        rx: &mut broadcast::Receiver<Arc<Event>>,
        alive: &AliveTracker,
        set: &SubscriberSet,
    ) {
        loop {
            match rx.try_recv() {
                Ok(ev) => Self::distribute(alive, set, ev).await,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    }

    /// Waits for the subscriber listener task to finish.
    ///
    /// If the listener was never started, this is a no-op.
    fn subscriber_listener(&self) {
        let mut rx = self.bus.subscribe();
        let set = Arc::clone(&self.subs);
        let alive = Arc::clone(&self.alive);
        let registry = Arc::clone(&self.registry);
        let rt = self.runtime_token.clone();

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;

                    msg = rx.recv() => match msg {
                        Ok(arc_ev) => {
                            if let Err(panic) = crate::core::panic_guard::guarded(
                                Self::distribute(&alive, &set, arc_ev),
                            )
                            .await
                            {
                                set.emit_arc(Arc::new(Event::subscriber_panicked(
                                    "subscriber_listener",
                                    format!("listener panic: {panic}"),
                                )));
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            let arc_e = Arc::new(Event::subscriber_overflow(
                                "subscriber_listener",
                                format!("lagged({skipped})"),
                            ));
                            alive.update(&arc_e).await;
                            set.emit_arc(arc_e);

                            let live: std::collections::HashSet<TaskId> = registry
                                .list()
                                .await
                                .into_iter()
                                .map(|(id, _)| id)
                                .collect();
                            alive.reconcile(&live).await;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },

                    _ = rt.cancelled() => {
                        Self::drain_pending(&mut rx, &alive, &set).await;
                        break;
                    }
                }
            }
        });

        *self.subscriber_handle.lock().unwrap() = Some(handle);
    }

    /// Awaits the subscriber listener.
    async fn join_subscriber_listener(&self) {
        let handle = self.subscriber_handle.lock().unwrap().take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    /// Drives static-mode completion.
    ///
    /// Waits for either:
    /// - an OS shutdown signal,
    /// - natural completion when the registry becomes empty.
    ///
    /// Both paths join registry/subscriber listeners and close subscribers before returning.
    async fn drive_shutdown(&self) -> Result<(), RuntimeError> {
        let res = tokio::select! {
            sig = crate::core::shutdown::wait_for_shutdown_signal() => self.on_shutdown_signal(sig).await,
            _ = self.registry.wait_until_empty() => self.drain_with_grace().await,
        };
        self.runtime_token.cancel();
        self.registry.join_listener().await;
        self.join_subscriber_listener().await;
        self.subs.close().await;
        res
    }

    /// Handles the result of OS shutdown-signal setup/waiting.
    ///
    /// A real signal publishes `ShutdownRequested` and starts graceful drain.
    /// Signal setup errors are returned as [`RuntimeError::SignalSetupFailed`] and are not treated as shutdown requests.
    async fn on_shutdown_signal(&self, res: std::io::Result<()>) -> Result<(), RuntimeError> {
        match res {
            Ok(()) => {
                self.bus.publish(Event::new(EventKind::ShutdownRequested));
                self.drain_with_grace().await
            }
            Err(e) => {
                let _ = self.close_admission_and_fence_registry().await;
                Err(RuntimeError::SignalSetupFailed { source: e })
            }
        }
    }

    /// Cancels tasks and waits for them within the configured grace window.
    ///
    /// Admission closes first. The registry processes every command accepted
    /// before that point, then this drains registered tasks and waits for
    /// detached join reporters using the remaining grace.
    /// Tasks/joiners that do not finish are returned as `GraceExceeded` stuck labels.
    async fn drain_with_grace(&self) -> Result<(), RuntimeError> {
        self.close_admission_and_fence_registry().await?;
        let grace = self.cfg.grace;
        let deadline = tokio::time::Instant::now() + grace;
        let mut stuck = self.registry.cancel_all_within(grace).await;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        stuck.extend(self.registry.wait_joins_within(remaining).await);
        if stuck.is_empty() {
            self.bus
                .publish(Event::new(EventKind::AllStoppedWithinGrace));
            Ok(())
        } else {
            self.bus.publish(Event::new(EventKind::GraceExceeded));
            Err(RuntimeError::GraceExceeded { grace, stuck })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscribers::Subscribe;
    use std::{future::Future, pin::Pin, sync::Mutex, task::Poll};

    struct RecordingSub {
        seen: Arc<Mutex<Vec<Event>>>,
    }
    impl RecordingSub {
        fn new() -> (Arc<Self>, Arc<Mutex<Vec<Event>>>) {
            let seen = Arc::new(Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    seen: Arc::clone(&seen),
                }),
                seen,
            )
        }
    }
    impl Subscribe for RecordingSub {
        fn on_event(&self, e: &Event) {
            self.seen.lock().unwrap().push(e.clone());
        }
        fn name(&self) -> &str {
            "recorder"
        }
        fn queue_capacity(&self) -> usize {
            8192
        }
    }

    fn core(cfg: SupervisorConfig) -> Arc<SupervisorCore> {
        core_with_subs(cfg, Vec::new())
    }

    fn core_with_subs(
        cfg: SupervisorConfig,
        subs: Vec<Arc<dyn crate::subscribers::Subscribe>>,
    ) -> Arc<SupervisorCore> {
        let bus = Bus::new(cfg.bus_capacity_clamped());
        let subs = Arc::new(SubscriberSet::new(subs, bus.clone()));
        let token = CancellationToken::new();
        let (cmd_tx, cmd_rx) = mpsc::channel(cfg.registry_queue_capacity_clamped());
        let registry = Registry::new(bus.clone(), token.clone(), None, cfg.grace, cmd_rx);
        let alive = Arc::new(AliveTracker::new());
        SupervisorCore::new_internal(cfg, bus, subs, alive, registry, token, cmd_tx)
    }

    async fn assert_pending_once<F: Future>(mut future: Pin<&mut F>) {
        std::future::poll_fn(|cx| match future.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("future completed before the expected ordering point"),
        })
        .await;
    }

    #[tokio::test]
    async fn subscriber_listener_reports_bus_lag_as_overflow() {
        let (recorder, seen) = RecordingSub::new();

        let cfg = SupervisorConfig {
            bus_capacity: 2,
            ..Default::default()
        };
        let core = core_with_subs(cfg, vec![recorder]);
        core.start();

        for i in 0..500 {
            core.bus
                .publish(Event::new(EventKind::TaskStarting).with_task(format!("f{i}")));
        }

        let saw_lag = timeout(Duration::from_secs(2), async {
            loop {
                let hit = seen.lock().unwrap().iter().any(|e| {
                    e.kind == EventKind::SubscriberOverflow
                        && e.reason
                            .as_deref()
                            .is_some_and(|r| r.starts_with("lagged("))
                });
                if hit {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or(false);

        let _ = core.shutdown().await;
        assert!(
            saw_lag,
            "subscriber_listener must report bus lag as SubscriberOverflow(lagged(n))"
        );
    }

    #[tokio::test]
    async fn drain_pending_delivers_retained_tail_after_a_lag_gap() {
        let (recorder, seen) = RecordingSub::new();

        let bus = Bus::new(2);
        let mut rx = bus.subscribe();
        let set = Arc::new(SubscriberSet::new(vec![recorder], bus.clone()));
        let alive = AliveTracker::new();

        for i in 0..5 {
            bus.publish(Event::new(EventKind::TaskStarting).with_task(format!("t{i}")));
        }

        SupervisorCore::drain_pending(&mut rx, &alive, &set).await;
        set.close().await;

        let delivered = seen.lock().unwrap();
        assert!(
            delivered
                .iter()
                .any(|e| e.kind == EventKind::TaskStarting && e.task.as_deref() == Some("t4")),
            "newest retained event must reach subscribers despite a lag gap"
        );
    }

    #[tokio::test]
    async fn natural_completion_publishes_all_stopped_within_grace() {
        use crate::{TaskContext, TaskFn, TaskRef};

        let (recorder, seen) = RecordingSub::new();
        let core = core_with_subs(SupervisorConfig::default(), vec![recorder]);

        let task: TaskRef = TaskFn::arc("done", |_ctx: TaskContext| async move { Ok(()) });
        let res = timeout(Duration::from_secs(5), core.run(vec![TaskSpec::once(task)])).await;
        assert!(
            matches!(res, Ok(Ok(()))),
            "natural completion must return Ok, got {res:?}"
        );

        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .any(|e| e.kind == EventKind::AllStoppedWithinGrace),
            "natural-completion success must publish a terminal verdict (AllStoppedWithinGrace)"
        );
    }

    #[tokio::test]
    async fn run_is_single_shot() {
        let core = core(SupervisorConfig::default());

        let first = timeout(Duration::from_secs(5), core.run(vec![])).await;
        assert!(
            matches!(first, Ok(Ok(()))),
            "first run must succeed, got {first:?}"
        );

        let second = core.run(vec![]).await;
        assert!(
            matches!(second, Err(RuntimeError::AlreadyRunning)),
            "second run() must return AlreadyRunning, got {second:?}"
        );
    }

    #[tokio::test]
    async fn add_is_rejected_once_shutting_down() {
        use crate::{TaskContext, TaskFn, TaskRef};

        let core = core(SupervisorConfig::default());
        core.start();

        let early: TaskRef = TaskFn::arc("early", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        assert!(core.add_task(TaskSpec::restartable(early)).await.is_ok());

        core.mark_shutting_down();

        let late: TaskRef = TaskFn::arc("late", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        let res = core.add_task(TaskSpec::restartable(late)).await;
        assert!(
            matches!(res, Err(RuntimeError::ShuttingDown)),
            "add() after shutdown began must be rejected, got {res:?}"
        );

        let _ = core.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_fence_processes_committed_add_before_drain() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::{TaskContext, TaskError, TaskFn, TaskRef};

        let cfg = SupervisorConfig {
            grace: Duration::from_secs(1),
            registry_queue_capacity: 1,
            ..Default::default()
        };
        let core = core(cfg);
        let accepted_id = TaskId::next();
        let accepted: TaskRef =
            TaskFn::arc("accepted-before-shutdown", |ctx: TaskContext| async move {
                ctx.cancelled().await;
                Err(TaskError::Canceled)
            });
        let (outcome, outcome_rx) = oneshot::channel();
        let (_, add_reply) = core
            .enqueue_add_task(accepted_id, TaskSpec::restartable(accepted), Some(outcome))
            .expect("the Add command must be committed before shutdown starts");

        let mut shutdown = Box::pin(core.shutdown());
        assert_pending_once(shutdown.as_mut()).await;
        assert!(core.is_shutting_down());

        let late_runs = Arc::new(AtomicUsize::new(0));
        let late_runs_by_task = Arc::clone(&late_runs);
        let late: TaskRef = TaskFn::arc("rejected-after-shutdown", move |_ctx: TaskContext| {
            late_runs_by_task.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        });
        assert!(matches!(
            core.add_task(TaskSpec::once(late)).await,
            Err(RuntimeError::ShuttingDown)
        ));

        core.start();
        assert!(matches!(
            timeout(Duration::from_secs(2), add_reply)
                .await
                .expect("the accepted Add must receive its registry reply"),
            Ok(Ok(()))
        ));
        timeout(Duration::from_secs(2), shutdown)
            .await
            .expect("shutdown must pass the fence and finish")
            .expect("the accepted cooperative task must drain cleanly");
        timeout(Duration::from_secs(2), outcome_rx)
            .await
            .expect("the accepted watched task must receive a terminal outcome")
            .expect("the registry must keep the watched outcome sender");

        assert!(!core.contains_id(accepted_id).await);
        assert_eq!(late_runs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unpolled_backpressured_add_does_not_block_shutdown_fence() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::{TaskContext, TaskFn, TaskRef};

        let cfg = SupervisorConfig {
            grace: Duration::from_secs(1),
            registry_queue_capacity: 1,
            ..Default::default()
        };
        let core = core(cfg);
        let mut events = core.bus.subscribe();
        let filler_reply = core
            .enqueue_remove(TaskId::next(), None)
            .expect("the filler must occupy the command queue");

        let rejected_id = TaskId::next();
        let runs = Arc::new(AtomicUsize::new(0));
        let runs_by_task = Arc::clone(&runs);
        let rejected: TaskRef =
            TaskFn::arc("backpressured-at-shutdown", move |_ctx: TaskContext| {
                runs_by_task.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            });
        let mut add =
            Box::pin(core.enqueue_add_task_wait(rejected_id, TaskSpec::once(rejected), None));
        assert_pending_once(add.as_mut()).await;

        let mut shutdown = Box::pin(core.shutdown());
        assert_pending_once(shutdown.as_mut()).await;
        assert!(core.is_shutting_down());

        core.start();
        timeout(Duration::from_secs(2), shutdown)
            .await
            .expect("the control fence must not wait for the backpressured Add")
            .expect("an empty registry must shut down cleanly");
        assert!(matches!(
            timeout(Duration::from_secs(2), filler_reply)
                .await
                .expect("the filler must receive its registry reply"),
            Ok(Ok(false))
        ));
        assert!(matches!(
            timeout(Duration::from_secs(2), add)
                .await
                .expect("the backpressured Add must wake after admission closes"),
            Err((RuntimeError::ShuttingDown, None))
        ));

        assert_eq!(runs.load(Ordering::SeqCst), 0);
        while let Ok(event) = events.try_recv() {
            assert!(
                event.id != Some(rejected_id) || event.kind != EventKind::TaskAddRequested,
                "an Add rejected behind the admission gate must stay silent"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confirmed_add_waits_for_capacity_and_registry_reply() {
        use crate::{TaskContext, TaskFn, TaskRef};

        let cfg = SupervisorConfig {
            registry_queue_capacity: 1,
            ..Default::default()
        };
        let core = core(cfg);
        let filler_reply = core
            .enqueue_remove(TaskId::next(), None)
            .expect("the filler must occupy the only queue slot");

        let task: TaskRef = TaskFn::arc("backpressured-add", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        let mut add = Box::pin(core.add_task(TaskSpec::restartable(task)));
        assert_pending_once(add.as_mut()).await;
        assert!(core.id_for_label("backpressured-add").await.is_none());

        core.start();
        assert!(matches!(
            timeout(Duration::from_secs(2), filler_reply)
                .await
                .expect("filler reply must resolve"),
            Ok(Ok(false))
        ));
        let id = timeout(Duration::from_secs(2), add)
            .await
            .expect("add must wake after capacity is released")
            .expect("registry must accept the task");
        assert!(core.contains_id(id).await);

        let _ = core.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confirmed_remove_waits_for_capacity_and_registry_reply() {
        let cfg = SupervisorConfig {
            registry_queue_capacity: 1,
            ..Default::default()
        };
        let core = core(cfg);
        let mut events = core.bus.subscribe();
        let filler_id = TaskId::next();
        let filler_reply = core
            .enqueue_remove(filler_id, None)
            .expect("the filler must occupy the only queue slot");

        let remove_id = TaskId::next();
        let mut remove = Box::pin(core.remove(remove_id));
        assert_pending_once(remove.as_mut()).await;
        while let Ok(event) = events.try_recv() {
            assert!(
                event.id != Some(remove_id) || event.kind != EventKind::TaskRemoveRequested,
                "a backpressured Remove is not visible before queue admission"
            );
        }

        core.start();
        assert!(matches!(
            timeout(Duration::from_secs(2), filler_reply)
                .await
                .expect("filler reply must resolve"),
            Ok(Ok(false))
        ));
        assert!(
            !timeout(Duration::from_secs(2), remove)
                .await
                .expect("Remove must wake after capacity is released")
                .expect("the registry must reply for an unknown id")
        );
        assert!(std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
            event.id == Some(remove_id) && event.kind == EventKind::TaskRemoveRequested
        }));

        let _ = core.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn try_remove_waits_for_registry_decision_after_admission() {
        let core = core(SupervisorConfig::default());
        let id = TaskId::next();
        let mut remove = Box::pin(core.try_remove(id));
        assert_pending_once(remove.as_mut()).await;

        core.start();
        assert!(
            !timeout(Duration::from_secs(2), remove)
                .await
                .expect("try_remove must wait for registry processing")
                .expect("an admitted try_remove must receive a reply")
        );

        let _ = core.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn remove_by_label_orders_after_an_already_queued_add() {
        use crate::{TaskContext, TaskFn, TaskRef};

        let core = core(SupervisorConfig::default());
        let mut events = core.bus.subscribe();
        let release = Arc::new(tokio::sync::Notify::new());
        let task_release = Arc::clone(&release);
        let task: TaskRef = TaskFn::arc("ordered-label", move |_ctx: TaskContext| {
            let release = Arc::clone(&task_release);
            async move {
                release.notified().await;
                Ok(())
            }
        });
        let id = TaskId::next();
        let (_, add_reply) = core
            .enqueue_add_task(id, TaskSpec::restartable(task), None)
            .expect("the Add command must enter the queue first");

        let mut remove = Box::pin(core.remove_by_label(Arc::from("ordered-label")));
        assert_pending_once(remove.as_mut()).await;
        core.start();

        assert!(matches!(
            timeout(Duration::from_secs(2), add_reply)
                .await
                .expect("Add reply must resolve"),
            Ok(Ok(()))
        ));
        assert!(
            timeout(Duration::from_secs(2), remove)
                .await
                .expect("label Remove must resolve")
                .expect("label Remove must receive a registry reply"),
            "the label lookup must happen after the queued Add is committed"
        );
        assert!(std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
            event.kind == EventKind::TaskRemoveRequested
                && event.id == Some(id)
                && event.task.as_deref() == Some("ordered-label")
        }));

        release.notify_one();
        timeout(Duration::from_secs(2), core.registry.wait_until_empty())
            .await
            .expect("the removed task must finish");
        let _ = core.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_by_label_orders_after_an_already_queued_add() {
        use crate::{TaskContext, TaskFn, TaskRef};

        let core = core(SupervisorConfig::default());
        let mut events = core.bus.subscribe();
        let task: TaskRef = TaskFn::arc("ordered-cancel-label", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        let id = TaskId::next();
        let (_, add_reply) = core
            .enqueue_add_task(id, TaskSpec::restartable(task), None)
            .expect("the Add command must enter the queue first");

        let mut cancel = Box::pin(core.cancel_by_label(Arc::from("ordered-cancel-label")));
        assert_pending_once(cancel.as_mut()).await;
        core.start();

        assert!(matches!(
            timeout(Duration::from_secs(2), add_reply)
                .await
                .expect("Add reply must resolve"),
            Ok(Ok(()))
        ));
        assert!(
            timeout(Duration::from_secs(2), cancel)
                .await
                .expect("label Cancel must resolve after terminal cleanup")
                .expect("label Cancel must receive a registry reply"),
            "the label lookup must happen after the queued Add is committed"
        );
        assert!(std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
            event.kind == EventKind::TaskRemoveRequested
                && event.id == Some(id)
                && event.task.as_deref() == Some("ordered-cancel-label")
                && event.reason.as_deref() == Some("manual_cancel")
        }));
        assert!(!core.contains_id(id).await);

        let _ = core.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn backpressured_remove_returns_shutting_down_without_request_event() {
        let cfg = SupervisorConfig {
            registry_queue_capacity: 1,
            ..Default::default()
        };
        let core = core(cfg);
        let mut events = core.bus.subscribe();
        let filler_reply = core
            .enqueue_remove(TaskId::next(), None)
            .expect("the filler must occupy the only queue slot");
        let remove_id = TaskId::next();
        let mut remove = Box::pin(core.remove(remove_id));
        assert_pending_once(remove.as_mut()).await;

        core.runtime_token.cancel();
        core.start();
        assert!(matches!(
            timeout(Duration::from_secs(2), remove)
                .await
                .expect("closing the queue must wake Remove"),
            Err(RuntimeError::ShuttingDown)
        ));
        let _ = timeout(Duration::from_secs(2), filler_reply)
            .await
            .expect("the buffered filler must still resolve");
        core.registry.join_listener().await;
        while let Ok(event) = events.try_recv() {
            assert!(
                event.id != Some(remove_id) || event.kind != EventKind::TaskRemoveRequested,
                "a Remove rejected before enqueue must not publish TaskRemoveRequested"
            );
        }

        let _ = core.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn backpressured_add_returns_shutting_down_when_queue_closes() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::{TaskContext, TaskFn, TaskRef};

        let cfg = SupervisorConfig {
            registry_queue_capacity: 1,
            ..Default::default()
        };
        let core = core(cfg);
        let filler_reply = core
            .enqueue_remove(TaskId::next(), None)
            .expect("the filler must occupy the only queue slot");

        let runs = Arc::new(AtomicUsize::new(0));
        let task_runs = Arc::clone(&runs);
        let task: TaskRef = TaskFn::arc("closed-while-waiting", move |_ctx: TaskContext| {
            task_runs.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        });
        let mut add = Box::pin(core.add_task(TaskSpec::once(task)));
        assert_pending_once(add.as_mut()).await;

        core.runtime_token.cancel();
        core.start();
        assert!(matches!(
            timeout(Duration::from_secs(2), add)
                .await
                .expect("closing the queue must wake the waiting Add"),
            Err(RuntimeError::ShuttingDown)
        ));
        let _ = timeout(Duration::from_secs(2), filler_reply)
            .await
            .expect("the buffered filler must still resolve");
        core.registry.join_listener().await;
        assert!(core.id_for_label("closed-while-waiting").await.is_none());
        assert_eq!(runs.load(Ordering::SeqCst), 0);

        let _ = core.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn try_add_reports_full_without_event_or_task_start() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::{TaskContext, TaskFn, TaskRef};

        let cfg = SupervisorConfig {
            registry_queue_capacity: 1,
            ..Default::default()
        };
        let core = core(cfg);
        let mut events = core.bus.subscribe();
        let filler_reply = core
            .enqueue_remove(TaskId::next(), None)
            .expect("the filler must occupy the only queue slot");

        let runs = Arc::new(AtomicUsize::new(0));
        let task_runs = Arc::clone(&runs);
        let task: TaskRef = TaskFn::arc("try-add-full", move |_ctx: TaskContext| {
            task_runs.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        });
        assert!(matches!(
            core.try_add_task(TaskSpec::once(task)).await,
            Err(RuntimeError::CommandQueueFull)
        ));
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert!(core.id_for_label("try-add-full").await.is_none());
        while let Ok(event) = events.try_recv() {
            assert!(
                event.kind != EventKind::TaskAddRequested
                    || event.task.as_deref() != Some("try-add-full"),
                "an Add rejected before enqueue must not publish TaskAddRequested"
            );
        }

        core.start();
        let _ = timeout(Duration::from_secs(2), filler_reply)
            .await
            .expect("filler reply must resolve");
        let _ = core.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn try_add_waits_for_registry_decision_after_admission() {
        use crate::{TaskContext, TaskFn, TaskRef};

        let core = core(SupervisorConfig::default());
        let task: TaskRef = TaskFn::arc("try-add-confirmed", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        let mut add = Box::pin(core.try_add_task(TaskSpec::restartable(task)));
        assert_pending_once(add.as_mut()).await;
        assert!(core.id_for_label("try-add-confirmed").await.is_none());

        core.start();
        let id = timeout(Duration::from_secs(2), add)
            .await
            .expect("try_add must resolve after the registry processes its command")
            .expect("registry must accept the task");
        assert!(core.contains_id(id).await);

        let _ = core.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_add_before_enqueue_rolls_back_admission() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::{TaskContext, TaskFn, TaskRef};

        let cfg = SupervisorConfig {
            registry_queue_capacity: 1,
            ..Default::default()
        };
        let core = core(cfg);
        let filler_reply = core
            .enqueue_remove(TaskId::next(), None)
            .expect("the filler must occupy the only queue slot");

        let runs = Arc::new(AtomicUsize::new(0));
        let task_runs = Arc::clone(&runs);
        let task: TaskRef = TaskFn::arc("dropped-before-enqueue", move |_ctx: TaskContext| {
            task_runs.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        });
        let mut add = Box::pin(core.add_task(TaskSpec::once(task)));
        assert_pending_once(add.as_mut()).await;
        drop(add);

        core.start();
        let _ = timeout(Duration::from_secs(2), filler_reply)
            .await
            .expect("filler reply must resolve");
        assert!(core.id_for_label("dropped-before-enqueue").await.is_none());
        assert_eq!(runs.load(Ordering::SeqCst), 0);

        let _ = core.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_add_after_enqueue_does_not_roll_command_back() {
        use crate::{TaskContext, TaskFn, TaskRef};

        let core = core(SupervisorConfig::default());
        let (started_tx, started_rx) = oneshot::channel();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let task_started = Arc::clone(&started_tx);
        let task: TaskRef = TaskFn::arc("dropped-after-enqueue", move |ctx: TaskContext| {
            let task_started = Arc::clone(&task_started);
            async move {
                if let Some(tx) = task_started.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                ctx.cancelled().await;
                Ok(())
            }
        });

        let mut add = Box::pin(core.add_task(TaskSpec::once(task)));
        assert_pending_once(add.as_mut()).await;
        drop(add);

        core.start();
        timeout(Duration::from_secs(2), started_rx)
            .await
            .expect("the queued task must start after its caller is dropped")
            .expect("the task must signal start");
        assert!(core.id_for_label("dropped-after-enqueue").await.is_some());

        let _ = core.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bounded_command_queue_reports_full_and_recovers_capacity() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::{TaskContext, TaskFn, TaskOutcome, TaskRef};

        let cfg = SupervisorConfig {
            registry_queue_capacity: 1,
            ..Default::default()
        };
        let core = core(cfg);
        let mut events = core.bus.subscribe();

        let filler_reply = core
            .enqueue_remove(TaskId::next(), None)
            .expect("the first command must fill the only queue slot");

        let runs = Arc::new(AtomicUsize::new(0));
        let rejected_runs = Arc::clone(&runs);
        let rejected: TaskRef = TaskFn::arc("queue-full-add", move |_ctx: TaskContext| {
            rejected_runs.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        });
        let rejected_id = TaskId::next();
        let (outcome, outcome_rx) = oneshot::channel();
        let full_add = core.enqueue_add_task(rejected_id, TaskSpec::once(rejected), Some(outcome));
        match full_add {
            Err((RuntimeError::CommandQueueFull, Some(returned))) => {
                returned
                    .send(TaskOutcome::Rejected {
                        reason: Arc::from("command_queue_full"),
                    })
                    .expect("the full command must return its outcome sender");
            }
            other => panic!("second command must report CommandQueueFull, got {other:?}"),
        }
        assert!(matches!(
            outcome_rx.await,
            Ok(TaskOutcome::Rejected { reason }) if reason.as_ref() == "command_queue_full"
        ));
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert!(!core.contains_id(rejected_id).await);
        while let Ok(event) = events.try_recv() {
            assert!(
                event.id != Some(rejected_id) || event.kind != EventKind::TaskAddRequested,
                "a command rejected before enqueue must not publish TaskAddRequested"
            );
        }

        let rejected_remove_id = TaskId::next();
        assert!(matches!(
            core.try_remove(rejected_remove_id).await,
            Err(RuntimeError::CommandQueueFull)
        ));
        while let Ok(event) = events.try_recv() {
            assert!(
                event.id != Some(rejected_remove_id)
                    || event.kind != EventKind::TaskRemoveRequested,
                "a command rejected before enqueue must not publish TaskRemoveRequested"
            );
        }

        let rejected_cancel_id = TaskId::next();
        assert!(matches!(
            core.cancel(rejected_cancel_id).await,
            Err(RuntimeError::CommandQueueFull)
        ));
        while let Ok(event) = events.try_recv() {
            assert!(
                event.id != Some(rejected_cancel_id)
                    || event.kind != EventKind::TaskRemoveRequested,
                "a Cancel rejected before enqueue must not publish TaskRemoveRequested"
            );
        }

        core.start();
        assert!(matches!(
            timeout(Duration::from_secs(2), filler_reply)
                .await
                .expect("filler reply must resolve"),
            Ok(Ok(false))
        ));

        let accepted: TaskRef = TaskFn::arc("capacity-recovered", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        let accepted_id = TaskId::next();
        let (_, accepted_reply) = core
            .enqueue_add_task(accepted_id, TaskSpec::restartable(accepted), None)
            .expect("capacity must recover after the filler is received");
        assert!(matches!(
            timeout(Duration::from_secs(2), accepted_reply)
                .await
                .expect("accepted add reply must resolve"),
            Ok(Ok(()))
        ));
        assert!(core.contains_id(accepted_id).await);

        let _ = core.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn static_run_backpressures_initial_batch_larger_than_queue() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::{TaskContext, TaskFn, TaskRef};

        let cfg = SupervisorConfig {
            registry_queue_capacity: 1,
            ..Default::default()
        };
        let core = core(cfg);
        let runs = Arc::new(AtomicUsize::new(0));
        let tasks = (0..4)
            .map(|index| {
                let runs = Arc::clone(&runs);
                let task: TaskRef =
                    TaskFn::arc(format!("static-{index}"), move |_ctx: TaskContext| {
                        runs.fetch_add(1, Ordering::SeqCst);
                        async { Ok(()) }
                    });
                TaskSpec::once(task)
            })
            .collect();

        timeout(Duration::from_secs(2), core.run(tasks))
            .await
            .expect("static run must not block on its bounded initial queue")
            .expect("static run must not fail when its batch exceeds queue capacity");
        assert_eq!(runs.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn closed_command_queue_returns_shutting_down_and_watcher() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::{TaskContext, TaskFn, TaskOutcome, TaskRef};

        let core = core(SupervisorConfig::default());
        core.start();
        core.runtime_token.cancel();
        timeout(Duration::from_secs(2), core.registry.join_listener())
            .await
            .expect("registry listener must stop");
        let mut events = core.bus.subscribe();

        let remove_id = TaskId::next();
        assert!(matches!(
            core.remove(remove_id).await,
            Err(RuntimeError::ShuttingDown)
        ));
        while let Ok(event) = events.try_recv() {
            assert!(
                event.id != Some(remove_id) || event.kind != EventKind::TaskRemoveRequested,
                "a remove rejected by a closed queue must not publish TaskRemoveRequested"
            );
        }

        let runs = Arc::new(AtomicUsize::new(0));
        let rejected_runs = Arc::clone(&runs);
        let task: TaskRef = TaskFn::arc("closed-command", move |_ctx: TaskContext| {
            rejected_runs.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        });
        let (outcome, outcome_rx) = oneshot::channel();
        match core.enqueue_add_task(TaskId::next(), TaskSpec::once(task), Some(outcome)) {
            Err((RuntimeError::ShuttingDown, Some(returned))) => {
                returned
                    .send(TaskOutcome::Rejected {
                        reason: Arc::from("shutting_down"),
                    })
                    .expect("closed queue must return its outcome sender");
            }
            other => panic!("closed command queue must return ShuttingDown, got {other:?}"),
        }
        assert!(matches!(
            outcome_rx.await,
            Ok(TaskOutcome::Rejected { reason }) if reason.as_ref() == "shutting_down"
        ));
        assert_eq!(runs.load(Ordering::SeqCst), 0);

        let _ = core.shutdown().await;
    }

    #[cfg(feature = "controller")]
    #[tokio::test]
    async fn add_task_with_id_watched_returns_watcher_on_failure() {
        use crate::{TaskContext, TaskFn, TaskRef};

        let core = core(SupervisorConfig::default());
        core.mark_shutting_down(); // close the admission gate so the add fails

        let (tx, rx) = tokio::sync::oneshot::channel();
        let task: TaskRef = TaskFn::arc("x", |_ctx: TaskContext| async { Ok(()) });

        let res = core.add_task_with_id_watched(TaskId::next(), TaskSpec::once(task), Some(tx));
        match res {
            Err((RuntimeError::ShuttingDown, Some(returned))) => {
                returned
                    .send(crate::TaskOutcome::Rejected {
                        reason: Arc::from("rejected"),
                    })
                    .expect("returned watcher must still be live");
                assert!(matches!(rx.await, Ok(crate::TaskOutcome::Rejected { .. })));
            }
            other => panic!("add must hand the watcher back on failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn signal_setup_error_surfaces_as_runtime_error_not_shutdown() {
        let core = core(SupervisorConfig::default());
        core.start();
        let mut rx = core.bus.subscribe();

        let err = std::io::Error::other("signal registration failed");
        let out = core.on_shutdown_signal(Err(err)).await;

        assert!(
            matches!(out, Err(RuntimeError::SignalSetupFailed { .. })),
            "a signal-setup error must surface as SignalSetupFailed, got {out:?}"
        );

        let mut saw_shutdown = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev.kind, EventKind::ShutdownRequested) {
                saw_shutdown = true;
            }
        }
        assert!(
            !saw_shutdown,
            "a signal-setup error must NOT masquerade as a shutdown request"
        );
    }

    #[tokio::test]
    async fn real_signal_publishes_shutdown_requested() {
        let core = core(SupervisorConfig::default());
        core.start();
        let mut rx = core.bus.subscribe();

        let out = core.on_shutdown_signal(Ok(())).await;
        assert!(out.is_ok(), "a real signal drains gracefully: {out:?}");

        let mut saw_shutdown = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev.kind, EventKind::ShutdownRequested) {
                saw_shutdown = true;
            }
        }
        assert!(saw_shutdown, "a real signal must publish ShutdownRequested");
    }

    #[tokio::test]
    async fn cancel_uses_registry_completion_when_event_bus_lags() {
        use crate::{TaskContext, TaskFn, TaskRef};
        use tokio::sync::broadcast::error::TryRecvError;

        let cfg = SupervisorConfig {
            bus_capacity: 1,
            ..Default::default()
        };
        let core = core(cfg);
        core.start();

        let cancellation_seen = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let seen_by_task = Arc::clone(&cancellation_seen);
        let task_release = Arc::clone(&release);
        let task: TaskRef = TaskFn::arc("laggy-cancel", move |ctx: TaskContext| {
            let seen = Arc::clone(&seen_by_task);
            let release = Arc::clone(&task_release);
            async move {
                ctx.cancelled().await;
                seen.notify_one();
                release.notified().await;
                Ok(())
            }
        });
        let id = core
            .add_task(TaskSpec::restartable(task))
            .await
            .expect("add accepted");

        let mut stale_events = core.bus.subscribe();
        let receiver_count = core.bus.receiver_count();
        let mut cancel = Box::pin(core.cancel(id));
        tokio::select! {
            result = &mut cancel => panic!("cancel returned before actor termination: {result:?}"),
            _ = cancellation_seen.notified() => {}
        }
        assert_eq!(
            core.bus.receiver_count(),
            receiver_count,
            "cancel must not create a correctness receiver on the event bus"
        );
        assert_pending_once(cancel.as_mut()).await;

        for _ in 0..16 {
            core.bus
                .publish(Event::new(EventKind::TaskStarting).with_task("noise"));
        }
        assert!(
            matches!(stale_events.try_recv(), Err(TryRecvError::Lagged(_))),
            "the observer must lag in this regression setup"
        );

        release.notify_one();
        assert!(
            timeout(Duration::from_secs(2), cancel)
                .await
                .expect("cancel must finish after terminal cleanup")
                .expect("cancel must receive a registry result")
        );
        assert!(
            !core.contains_id(id).await,
            "terminal completion must follow registry state cleanup"
        );

        let _ = core.shutdown().await;
    }
}
