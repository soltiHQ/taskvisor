//! Controller task lifecycle and the serialized actor loop.

use std::{future::Future, sync::Arc};

use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::{
    controller::error::ControllerError,
    events::{Event, EventKind},
};

use super::{Controller, ControllerCommand, ControllerTask};

impl Controller {
    /// Starts the single owned controller loop.
    ///
    /// Later calls are no-ops. Runtime shutdown joins this exact task before cleanup completes.
    pub fn run(self: &Arc<Self>) {
        self.task.get_or_init(|| {
            let controller = Arc::clone(self);
            ControllerTask::new(tokio::spawn(async move {
                controller.run_task().await;
            }))
        });
    }

    /// Runs the controller loop behind its outer panic boundary and final state cleanup.
    async fn run_task(self: Arc<Self>) {
        let token = self.shutdown_token.clone();
        match crate::core::panic_guard::guarded(self.run_inner(token)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                self.bus.publish(
                    Event::new(EventKind::ControllerRejected)
                        .with_task("controller")
                        .with_reason(format!("controller_loop_exited: {error}")),
                );
            }
            Err(panic) => {
                self.bus.publish(
                    Event::new(EventKind::ControllerRejected)
                        .with_task("controller")
                        .with_reason(format!("controller_loop_panicked: {panic}")),
                );
            }
        }

        self.mark_shutting_down();
        self.finalize_remaining_watchers();
        self.slots.clear();
    }

    /// Waits for the owned controller loop exactly once.
    ///
    /// Concurrent and later callers share the stored join state.
    /// Returns `false` when the controller task did not join cleanly.
    pub(crate) async fn join(&self) -> bool {
        if let Some(task) = self.task.get() {
            task.join(&self.bus).await
        } else {
            true
        }
    }

    #[cfg(test)]
    pub(super) async fn is_joined(&self) -> bool {
        match self.task.get() {
            Some(task) => task.is_joined().await,
            None => false,
        }
    }

    /// Runs the controller event loop.
    ///
    /// The loop handles ordered controller commands, results from tracked registry workers, and the reliable runtime shutdown-start signal.
    /// Slot and queue transitions are applied in this loop.
    ///
    /// On shutdown, it closes the command receiver, drains buffered commands, and resolves pending submissions and identity-operation replies.
    pub(super) async fn run_inner(&self, token: CancellationToken) -> Result<(), ControllerError> {
        let mut rx = self
            .rx
            .write()
            .await
            .take()
            .ok_or(ControllerError::AlreadyStarted)?;

        let mut admissions = JoinSet::new();
        let mut completions = JoinSet::new();
        let mut removals = JoinSet::new();
        let mut identity_operations = JoinSet::new();
        let identity_operation_limit = self.config.queue_capacity().get();
        let loop_result = crate::core::panic_guard::guarded(async {
            loop {
                tokio::select! {
                biased;

                _ = token.cancelled() => {
                    self.mark_shutting_down();
                    break;
                },

                Some(command) = rx.recv(), if identity_operations.len() < identity_operation_limit => {
                    match command {
                        ControllerCommand::Submit(sub) => {
                            let _ = self
                                .guarded(
                                    "handle_submission",
                                    self.handle_submission(sub, &mut admissions, &mut removals),
                                )
                                .await;
                        }
                        ControllerCommand::ManageIdentity {
                            id,
                            operation,
                            reply,
                        } => {
                            let _ = self
                                .guarded(
                                    "handle_identity_operation",
                                    self.handle_identity_operation(
                                        id,
                                        operation,
                                        reply,
                                        &mut identity_operations,
                                    ),
                                )
                                .await;
                        }
                    }
                }
                result = admissions.join_next(), if !admissions.is_empty() => {
                    match result {
                        Some(Ok(result)) => {
                            let _ = self
                                .guarded(
                                    "handle_admission_result",
                                    self.handle_admission_result(
                                        result,
                                        &mut admissions,
                                        &mut completions,
                                        &mut removals,
                                    ),
                                )
                                .await;
                        }
                        Some(Err(error)) => {
                            self.bus.publish(
                                Event::new(EventKind::ControllerRejected)
                                    .with_task("controller")
                                    .with_reason(format!("admission_waiter_failed: {error}")),
                            );
                        }
                        None => {}
                    }
                }
                result = completions.join_next(), if !completions.is_empty() => {
                    match result {
                        Some(Ok(result)) => {
                            let _ = self
                                .guarded(
                                    "handle_completion_result",
                                    self.handle_completion_result(result, &mut admissions),
                                )
                                .await;
                        }
                        Some(Err(error)) => {
                            self.bus.publish(
                                Event::new(EventKind::ControllerRejected)
                                    .with_task("controller")
                                    .with_reason(format!("completion_waiter_failed: {error}")),
                            );
                        }
                        None => {}
                    }
                }
                result = removals.join_next(), if !removals.is_empty() => {
                    match result {
                        Some(Ok(result)) => {
                            let _ = self
                                .guarded(
                                    "handle_removal_result",
                                    self.handle_removal_result(result),
                                )
                                .await;
                        }
                        Some(Err(error)) => {
                            self.bus.publish(
                                Event::new(EventKind::ControllerRejected)
                                    .with_task("controller")
                                    .with_reason(format!("removal_waiter_failed: {error}")),
                            );
                        }
                        None => {}
                    }
                }
                result = identity_operations.join_next(), if !identity_operations.is_empty() => {
                    match result {
                        Some(Ok(())) => {}
                        Some(Err(error)) => {
                            self.bus.publish(
                                Event::new(EventKind::ControllerRejected)
                                    .with_task("controller")
                                    .with_reason(format!("identity_operation_failed: {error}")),
                            );
                        }
                        None => {}
                    }
                }
                }
            }
        })
        .await;
        self.finalize_pending_on_shutdown(&mut rx);
        admissions.abort_all();
        completions.abort_all();
        removals.abort_all();
        identity_operations.abort_all();
        Self::drain_workers(&mut admissions).await;
        Self::drain_workers(&mut completions).await;
        Self::drain_workers(&mut removals).await;
        Self::drain_workers(&mut identity_operations).await;
        self.finalize_slot_state_on_shutdown().await;
        if let Err(panic) = loop_result {
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task("controller")
                    .with_reason(format!("controller_loop_panicked: {panic}")),
            );
        }
        Ok(())
    }

    /// Runs one controller work unit behind a panic boundary.
    ///
    /// A panic is converted into a diagnostic `ControllerRejected` event and the loop continues.
    ///
    /// This guard does not repair partially updated slot state by itself.
    /// Callers that park watcher state must still make sure the watcher is resolved or returned on every failure path.
    /// Returns the work-unit output on success and `None` after a caught panic.
    pub(super) async fn guarded<T>(
        &self,
        who: &'static str,
        fut: impl Future<Output = T>,
    ) -> Option<T> {
        match crate::core::panic_guard::guarded(fut).await {
            Ok(output) => Some(output),
            Err(msg) => {
                self.bus.publish(
                    Event::new(EventKind::ControllerRejected)
                        .with_task("controller")
                        .with_reason(format!("{who}_panicked: {msg}")),
                );
                None
            }
        }
    }
}
