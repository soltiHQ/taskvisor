//! Owns the single-use lifecycle behind the supervisor's static run methods.
//!
//! [`Supervisor::run`](crate::Supervisor::run), [`Supervisor::run_until`](crate::Supervisor::run_until),
//! and [`Supervisor::run_with_os_signals`](crate::Supervisor::run_with_os_signals) converge here.
//! A non-empty initial task set reserves cleanup ownership as one batch and reaches the registry
//! in one atomic command. After successful admission, the workflow joins shared shutdown when its
//! trigger wins or the registry becomes empty.
//!
//! Failures before the workflow's explicit commit release the claim.
//! After that commit, no later static run call can own the lifecycle.

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use super::super::{SupervisorCore, shutdown_workflow::ShutdownTrigger};
use crate::{
    core::{deferred_drop, registry::AddBatchItem},
    error::RuntimeError,
    identity::TaskId,
    tasks::TaskSpec,
};

/// Rolls back the static-run claim unless the workflow commits it.
struct StaticRunClaim<'a> {
    /// Single-run flag borrowed from the runtime core.
    running: &'a AtomicBool,
    /// Prevents rollback after the lifecycle becomes externally visible.
    committed: bool,
}

impl<'a> StaticRunClaim<'a> {
    /// Claims the lifecycle without committing it.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::AlreadyRunning`] when another call owns the claim or an earlier call committed it.
    fn acquire(running: &'a AtomicBool) -> Result<Self, RuntimeError> {
        running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| RuntimeError::AlreadyRunning)?;
        Ok(Self {
            running,
            committed: false,
        })
    }

    /// Makes this lifecycle claim permanent.
    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for StaticRunClaim<'_> {
    /// Releases the claim only when the workflow never committed it.
    fn drop(&mut self) {
        if !self.committed {
            self.running.store(false, Ordering::Release);
        }
    }
}

impl SupervisorCore {
    /// Starts workers without committing the caller's static-run claim.
    ///
    /// # Errors
    ///
    /// Returns the startup errors from [`SupervisorCore::start`].
    /// The caller can retry a static run after this method fails.
    fn start_static_run(&self) -> Result<(), RuntimeError> {
        self.start()
    }

    /// Runs without adding an application or operating-system shutdown trigger.
    pub(crate) async fn run(self: &Arc<Self>, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
        self.run_until_trigger(tasks, std::future::pending()).await
    }

    /// Adds one application future as a shutdown trigger.
    pub(crate) async fn run_until<F>(
        self: &Arc<Self>,
        tasks: Vec<TaskSpec>,
        shutdown: F,
    ) -> Result<(), RuntimeError>
    where
        F: Future<Output = ()>,
    {
        self.run_until_trigger(tasks, async move {
            shutdown.await;
            ShutdownTrigger::Requested
        })
        .await
    }

    /// Adds Taskvisor's operating-system signal listener as a shutdown trigger.
    pub(crate) async fn run_with_os_signals(
        self: &Arc<Self>,
        tasks: Vec<TaskSpec>,
    ) -> Result<(), RuntimeError> {
        self.run_until_trigger(tasks, async {
            match crate::core::shutdown::wait_for_shutdown_signal().await {
                Ok(()) => ShutdownTrigger::Requested,
                Err(source) => ShutdownTrigger::SignalSetupFailed(Arc::new(source)),
            }
        })
        .await
    }

    /// Orders the lifecycle claim, batch admission, and shutdown races.
    ///
    /// Preflight failures and shutdown observed before non-empty ownership reservation release the claim.
    /// If the caller's trigger wins, this method starts the runtime and commits the claim. After ownership is
    /// reserved, successful runtime startup or an observed shared shutdown also commits it.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::AlreadyRunning`] when the lifecycle is already owned or committed.
    /// It also returns resource, startup, registry, signal-setup, and shared shutdown errors from the selected path.
    async fn run_until_trigger<F>(
        self: &Arc<Self>,
        tasks: Vec<TaskSpec>,
        shutdown: F,
    ) -> Result<(), RuntimeError>
    where
        F: Future<Output = ShutdownTrigger>,
    {
        let run_claim = StaticRunClaim::acquire(&self.running)?;

        if let Some(limit) = self.runtime_config().max_registered_tasks()
            && tasks.len() > limit.get()
        {
            return Err(RuntimeError::ResourceLimitReached {
                resource: "registered_tasks",
                limit: limit.get(),
            });
        }

        if tasks.len()
            > self
                .drop_domain()
                .capacity()
                .saturating_sub(self.subs.ownership_slots())
        {
            return Err(RuntimeError::ResourceLimitReached {
                resource: deferred_drop::OWNERSHIP_RESOURCE,
                limit: self.drop_domain().capacity(),
            });
        }

        if tasks.is_empty() {
            if self.is_shutting_down() {
                run_claim.commit();
                return self.wait_started_shutdown().await;
            }
            self.start_static_run()?;
            run_claim.commit();
            return self.drive_shutdown(shutdown).await;
        }

        tokio::pin!(shutdown);
        tokio::select! {
            biased;
            _ = self.shutdown.started.cancelled() => {
                return self.wait_started_shutdown().await;
            }
            trigger = shutdown.as_mut() => {
                self.start_static_run()?;
                run_claim.commit();
                return self.join_shutdown(trigger).await;
            }
            _ = std::future::ready(()) => {}
        }
        tokio::runtime::Handle::try_current().map_err(|_| RuntimeError::TokioRuntimeUnavailable)?;
        let reservations = self
            .try_reserve_ownership_many(tasks.len())
            .map_err(Self::ownership_admission_error)?;

        let tasks: Vec<_> = tasks
            .into_iter()
            .zip(reservations)
            .map(|(spec, reservation)| {
                let label = spec.shared_name();
                let owned = self.own_task(spec, reservation);
                (label, owned)
            })
            .collect();

        if self.is_shutting_down() {
            run_claim.commit();
            return self.wait_started_shutdown().await;
        }
        self.start_static_run()?;
        run_claim.commit();

        let items = tasks
            .into_iter()
            .map(|(label, owned)| AddBatchItem {
                id: TaskId::next(),
                label,
                owned,
            })
            .collect();
        let admission = tokio::select! {
            biased;
            _ = self.shutdown.started.cancelled() => {
                return self.wait_started_shutdown().await;
            }
            trigger = shutdown.as_mut() => {
                return self.join_shutdown(trigger).await;
            }
            admission = self.enqueue_add_batch_wait(items) => admission,
        };
        let reply = match admission {
            Ok(reply) => reply,
            Err(RuntimeError::ShuttingDown) if self.shutdown.started.is_cancelled() => {
                return self.wait_started_shutdown().await;
            }
            Err(error) => return Err(error),
        };

        match Self::await_add_batch_reply(reply).await {
            Ok(()) => self.drive_shutdown(shutdown).await,
            Err(RuntimeError::ShuttingDown) if self.shutdown.started.is_cancelled() => {
                self.wait_started_shutdown().await
            }
            Err(error) => Err(error),
        }
    }

    /// Lets shared shutdown, the caller trigger, or registry emptiness choose the owner.
    ///
    /// # Errors
    ///
    /// Returns the result of the shared shutdown workflow.
    async fn drive_shutdown<F>(self: &Arc<Self>, shutdown: F) -> Result<(), RuntimeError>
    where
        F: Future<Output = ShutdownTrigger>,
    {
        tokio::select! {
            _ = self.shutdown.started.cancelled() => self.wait_started_shutdown().await,
            trigger = shutdown => self.join_shutdown(trigger).await,
            _ = self.registry.wait_until_empty() => self.join_shutdown(ShutdownTrigger::Natural).await,
        }
    }
}
