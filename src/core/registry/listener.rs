//! Runs the registry's ordered command and completion loop.
//!
//! [`SupervisorCore`](crate::core::runtime::SupervisorCore) starts this listener
//! with the other runtime workers. The loop dispatches bounded management commands
//! to admission or removal. It receives actor completion identities and shutdown
//! fences on separate channels.
//!
//! Completion cleanup gets bounded bursts. The listener then gives command and
//! control input an explicit turn. Runtime cancellation closes intake, drains
//! buffered commands to direct decisions, claims remaining registered actors,
//! and waits for removal owners. Joining the listener also closes the actor
//! reaper coordinator.

use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use super::{
    Registry,
    protocol::{RegistryCommand, RegistryControl},
};
use crate::{error::RuntimeError, events::Event, identity::TaskId};

/// Completion cleanup gets a bounded burst before management and control input
/// receives an explicit turn.
pub(super) const COMPLETION_BURST_LIMIT: usize = 64;

/// Listener-owned channel endpoints and the single listener task handle.
pub(super) struct ListenerState {
    /// Receives ordered management commands after startup.
    cmd_rx: Mutex<Option<mpsc::Receiver<RegistryCommand>>>,
    /// Sends reliable control messages outside command backpressure.
    control_tx: mpsc::UnboundedSender<RegistryControl>,
    /// Receives reliable control messages after startup.
    control_rx: Mutex<Option<mpsc::UnboundedReceiver<RegistryControl>>>,
    /// Reports actor identities whose result is ready.
    pub(super) completion_tx: mpsc::UnboundedSender<TaskId>,
    /// Receives reliable actor completion identities after startup.
    completion_rx: Mutex<Option<mpsc::UnboundedReceiver<TaskId>>>,
    /// Owns the single registry listener task.
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl ListenerState {
    /// Creates listener channels around the configured management receiver.
    pub(super) fn new(cmd_rx: mpsc::Receiver<RegistryCommand>) -> Self {
        let (completion_tx, completion_rx) = mpsc::unbounded_channel();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        Self {
            cmd_rx: Mutex::new(Some(cmd_rx)),
            control_tx,
            control_rx: Mutex::new(Some(control_rx)),
            completion_tx,
            completion_rx: Mutex::new(Some(completion_rx)),
            handle: Mutex::new(None),
        }
    }
}

impl Registry {
    /// Waits for decisions on commands committed before the fence.
    ///
    /// This does not wait for actor joins or terminal membership cleanup.
    /// The control channel is independent of bounded management queue capacity.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::ShuttingDown`] when the listener no longer
    /// accepts control messages or cannot return the acknowledgement.
    pub(crate) async fn fence(&self) -> Result<(), RuntimeError> {
        let (reply, reply_rx) = oneshot::channel();
        self.listener
            .control_tx
            .send(RegistryControl::Fence { reply })
            .map_err(|_| RuntimeError::ShuttingDown)?;
        reply_rx.await.map_err(|_| RuntimeError::ShuttingDown)
    }

    /// Starts the listener and physical reaper coordinator once.
    ///
    /// # Panics
    ///
    /// Panics without an active Tokio runtime or when called more than once.
    pub fn spawn_listener(self: Arc<Self>) {
        self.actors.spawn();
        let mut cmd_rx = self
            .listener
            .cmd_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("spawn_listener called exactly once");
        let mut completion_rx = self
            .listener
            .completion_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("spawn_listener called exactly once");
        let mut control_rx = self
            .listener
            .control_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("spawn_listener called exactly once");

        let rt = self.runtime_token.clone();
        let me = self.clone();

        let handle = tokio::spawn(async move {
            let mut completion_burst = 0usize;
            loop {
                if rt.is_cancelled() {
                    break;
                }
                if completion_burst >= COMPLETION_BURST_LIMIT {
                    if let Ok(control) = control_rx.try_recv() {
                        me.handle_control(control, &mut cmd_rx).await;
                        completion_burst = 0;
                        continue;
                    }
                    if let Ok(command) = cmd_rx.try_recv() {
                        me.handle_command(command).await;
                        completion_burst = 0;
                        continue;
                    }
                    completion_burst = 0;
                }
                tokio::select! {
                    _ = rt.cancelled() => break,

                    completed = completion_rx.recv() => match completed {
                        Some(id) => {
                            me.guarded("registry", me.cleanup_completed_task(id)).await;
                            completion_burst = completion_burst.saturating_add(1);
                        }
                        None => break,
                    },

                    control = control_rx.recv() => match control {
                        Some(control) => {
                            me.handle_control(control, &mut cmd_rx).await;
                            completion_burst = 0;
                        }
                        None => break,
                    },

                    cmd = cmd_rx.recv() => match cmd {
                        Some(command) => {
                            me.handle_command(command).await;
                            completion_burst = 0;
                        }
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
            .listener
            .handle
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    /// Processes one management command to its direct registry decision.
    async fn handle_command(&self, command: RegistryCommand) {
        match command {
            RegistryCommand::Add {
                id,
                label,
                owned,
                outcome,
                completion,
                reply,
            } => {
                self.guarded(
                    "registry",
                    self.spawn_and_register(id, label, *owned, outcome, completion, reply),
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
            .listener
            .handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some(handle) = handle else {
            return true;
        };

        let listener_clean = match handle.await {
            Ok(()) => true,
            Err(error) => {
                self.bus.publish_lazy(|| {
                    Event::runtime_failure("registry", format!("listener join failed: {error}"))
                });
                false
            }
        };
        let actors_clean = self.actors.join().await;
        if !actors_clean {
            self.bus
                .publish_lazy(|| Event::runtime_failure("registry", "actor runtime join failed"));
        }
        listener_clean && actors_clean
    }

    /// Aborts the listener so shutdown join-failure handling can be tested.
    #[cfg(test)]
    pub(crate) fn abort_listener_for_test(&self) {
        if let Some(handle) = self
            .listener
            .handle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            handle.abort();
        }
    }

    /// Runs one listener operation under a panic boundary.
    ///
    /// A processing panic becomes a diagnostic event.
    /// The registry listener remains available.
    async fn guarded(&self, who: &'static str, fut: impl Future<Output = ()>) {
        if let Err(msg) = crate::core::panic_guard::guarded(fut).await {
            self.bus
                .publish_lazy(|| Event::runtime_failure(who, format!("listener panic: {msg}")));
        }
    }
}
