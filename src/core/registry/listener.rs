//! Registry listener lifecycle, control fences, and command dispatch.

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

/// Listener-owned channel endpoints and the single listener task handle.
pub(super) struct ListenerState {
    cmd_rx: Mutex<Option<mpsc::Receiver<RegistryCommand>>>,
    control_tx: mpsc::UnboundedSender<RegistryControl>,
    control_rx: Mutex<Option<mpsc::UnboundedReceiver<RegistryControl>>>,
    pub(super) completion_tx: mpsc::UnboundedSender<TaskId>,
    completion_rx: Mutex<Option<mpsc::UnboundedReceiver<TaskId>>>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl ListenerState {
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
    /// Waits until every management command committed before this call reaches its direct registry decision.
    ///
    /// This does not wait for actor joins or terminal membership cleanup.
    /// The control channel is independent of bounded management queue capacity.
    pub(crate) async fn fence(&self) -> Result<(), RuntimeError> {
        let (reply, reply_rx) = oneshot::channel();
        self.listener
            .control_tx
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
    /// On runtime cancellation, it closes the command receiver and processes commands already buffered without waiting for uncommitted reservations.
    /// It then claims entries still in `Registered` with zero additional grace and waits for both existing and newly created join owners to finish.
    pub fn spawn_listener(self: Arc<Self>) {
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
            .listener
            .handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some(handle) = handle else {
            return true;
        };

        match handle.await {
            Ok(()) => true,
            Err(error) => {
                self.bus.publish(Event::runtime_failure(
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
    /// A panic while processing one command or completion is reported as a diagnostic event instead of killing the registry listener.
    async fn guarded(&self, who: &'static str, fut: impl Future<Output = ()>) {
        if let Err(msg) = crate::core::panic_guard::guarded(fut).await {
            self.bus.publish(Event::runtime_failure(
                who,
                format!("listener panic: {msg}"),
            ));
        }
    }
}
