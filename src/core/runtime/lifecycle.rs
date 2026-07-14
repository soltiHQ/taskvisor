//! Runtime startup and static-run lifecycle.

use std::sync::{Arc, atomic::Ordering};

use super::{SupervisorCore, shutdown_workflow::ShutdownTrigger};
use crate::{core::registry::AddBatchItem, error::RuntimeError, identity::TaskId, tasks::TaskSpec};

impl SupervisorCore {
    /// Starts runtime workers and listeners.
    ///
    /// This starts:
    /// - subscriber queue workers,
    /// - the event relay,
    /// - the registry listener.
    ///
    /// Safe to call more than once.
    /// Concurrent callers wait for the first startup to install every listener before they return.
    pub(crate) fn start(&self) {
        if self.started.load(Ordering::Acquire) {
            return;
        }

        let _startup = self
            .startup_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.started.load(Ordering::Acquire) {
            return;
        }

        tokio::runtime::Handle::try_current()
            .expect("Supervisor::serve requires an active Tokio runtime");
        self.subs.start();
        self.subscriber_listener();
        self.registry.clone().spawn_listener();
        self.started.store(true, Ordering::Release);
    }

    /// Runs a static task set until explicit shutdown, an OS signal, or registry emptiness.
    ///
    /// This starts the runtime and, when the task set is non-empty, registers it as one atomic batch.
    /// It then drives shutdown or natural completion.
    ///
    /// Single-shot: a second or concurrent call returns [`RuntimeError::AlreadyRunning`].
    pub(crate) async fn run(self: &Arc<Self>, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
        if self.running.swap(true, Ordering::AcqRel) {
            return Err(RuntimeError::AlreadyRunning);
        }
        if self.is_shutting_down() {
            return self.wait_started_shutdown().await;
        }
        self.start();

        if tasks.is_empty() {
            return self.drive_shutdown().await;
        }

        let items = tasks
            .into_iter()
            .map(|spec| AddBatchItem {
                id: TaskId::next(),
                label: Arc::from(spec.task().name()),
                spec,
            })
            .collect();
        let reply = match self.enqueue_add_batch_wait(items).await {
            Ok(reply) => reply,
            Err(RuntimeError::ShuttingDown) if self.shutdown.started.is_cancelled() => {
                return self.wait_started_shutdown().await;
            }
            Err(error) => return Err(error),
        };

        match Self::await_add_batch_reply(reply).await {
            Ok(()) => self.drive_shutdown().await,
            Err(RuntimeError::ShuttingDown) if self.shutdown.started.is_cancelled() => {
                self.wait_started_shutdown().await
            }
            Err(error) => Err(error),
        }
    }

    /// Drives static-mode completion.
    ///
    /// Waits for either:
    /// - a shared shutdown already started by another entry point,
    /// - an OS shutdown signal,
    /// - natural completion when the registry becomes empty.
    ///
    /// Every path joins registry and subscriber listeners and closes subscribers before returning.
    async fn drive_shutdown(self: &Arc<Self>) -> Result<(), RuntimeError> {
        tokio::select! {
            _ = self.shutdown.started.cancelled() => self.wait_started_shutdown().await,
            sig = crate::core::shutdown::wait_for_shutdown_signal() => self.on_shutdown_signal(sig).await,
            _ = self.registry.wait_until_empty() => self.join_shutdown(ShutdownTrigger::Natural).await,
        }
    }

    /// Handles the result of OS shutdown-signal setup/waiting.
    ///
    /// If a received signal wins the shutdown race, it starts graceful shutdown and publishes `ShutdownRequested`.
    /// If a signal-setup error wins, the shared result is [`RuntimeError::SignalSetupFailed`];
    /// common cleanup still runs, but `ShutdownRequested` and the graceful task-drain verdict are not emitted.
    /// A losing signal result joins the operation already in progress.
    pub(super) async fn on_shutdown_signal(
        self: &Arc<Self>,
        res: std::io::Result<()>,
    ) -> Result<(), RuntimeError> {
        match res {
            Ok(()) => self.join_shutdown(ShutdownTrigger::Requested).await,
            Err(source) => {
                self.join_shutdown(ShutdownTrigger::SignalSetupFailed(Arc::new(source)))
                    .await
            }
        }
    }
}
