//! Runs and joins the serialized controller task.
//!
//! The driver polls ordered commands, tracked runtime results, and shutdown in one loop.
//! A burst limit gives command intake regular turns when internal results stay ready.
//! Panic boundaries keep one failed work item from stopping the loop.

use std::{future::Future, sync::Arc};

use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::events::Event;

use super::super::{Controller, ControllerCommand, TrackedOperations};
use super::ControllerTask;

/// Maximum internal-result burst before a non-blocking command check.
pub(super) const INTERNAL_RESULT_BURST_LIMIT: usize = 64;

impl Controller {
    /// Starts the single owned controller loop.
    ///
    /// Later calls are no-ops. Runtime shutdown joins this task before cleanup completes.
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
                self.bus.publish_lazy(|| {
                    Event::runtime_failure("controller", format!("controller_loop_exited: {error}"))
                });
            }
            Err(panic) => {
                self.bus.publish_lazy(|| {
                    Event::runtime_failure(
                        "controller",
                        format!("controller_loop_panicked: {panic}"),
                    )
                });
            }
        }

        self.mark_shutting_down();
        self.finalize_slot_state_on_shutdown().await;
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
    pub(in crate::controller::engine) async fn is_joined(&self) -> bool {
        match self.task.get() {
            Some(task) => task.is_joined().await,
            None => false,
        }
    }

    /// Consumes the command receiver and runs the controller loop.
    pub(in crate::controller::engine) async fn run_inner(
        &self,
        token: CancellationToken,
    ) -> Result<(), &'static str> {
        let mut rx = self
            .rx
            .write()
            .await
            .take()
            .ok_or("controller command receiver already taken")?;

        let mut operations = TrackedOperations::new(
            self.supervisor.clone(),
            self.config.admission_capacity().get(),
        );
        let loop_result = crate::core::panic_guard::guarded(async {
            let mut internal_result_burst = 0usize;
            loop {
                if token.is_cancelled() {
                    self.mark_shutting_down();
                    break;
                }

                if internal_result_burst >= INTERNAL_RESULT_BURST_LIMIT {
                    match rx.try_recv() {
                        Ok(command) => {
                            self.handle_controller_command(command, &mut operations).await;
                            internal_result_burst = 0;
                            continue;
                        }
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                            internal_result_burst = 0;
                        }
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                    }
                }

                tokio::select! {
                _ = token.cancelled() => {
                    self.mark_shutting_down();
                    break;
                },

                result = operations.capacity.next(), if !operations.capacity.is_empty() => {
                    internal_result_burst = internal_result_burst.saturating_add(1);
                    if let Some(result) = result {
                        let _ = self
                            .guarded(
                                "handle_registry_capacity_result",
                                self.handle_registry_capacity_result(
                                    result.id,
                                    result.decision,
                                    &mut operations,
                                ),
                            )
                            .await;
                    }
                }

                result = operations.admissions.next(), if !operations.admissions.is_empty() => {
                    internal_result_burst = internal_result_burst.saturating_add(1);
                    match result {
                        Some(Ok(result)) => {
                            let _ = self
                                .guarded(
                                    "handle_admission_result",
                                    self.handle_admission_result(result, &mut operations),
                                )
                                .await;
                        }
                        Some(Err(error)) => {
                            self.bus.publish_lazy(|| {
                                Event::runtime_failure(
                                    "controller",
                                    format!("admission_waiter_failed: {error}"),
                                )
                            });
                        }
                        None => {}
                    }
                }
                result = operations.completions.next(), if !operations.completions.is_empty() => {
                    internal_result_burst = internal_result_burst.saturating_add(1);
                    match result {
                        Some(Ok(result)) => {
                            let _ = self
                                .guarded(
                                    "handle_completion_result",
                                    self.handle_completion_result(result, &mut operations),
                                )
                                .await;
                        }
                        Some(Err(error)) => {
                            self.bus.publish_lazy(|| {
                                Event::runtime_failure(
                                    "controller",
                                    format!("completion_waiter_failed: {error}"),
                                )
                            });
                        }
                        None => {}
                    }
                }
                result = operations.removals.next(), if !operations.removals.is_empty() => {
                    internal_result_burst = internal_result_burst.saturating_add(1);
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
                            self.bus.publish_lazy(|| {
                                Event::runtime_failure(
                                    "controller",
                                    format!("removal_waiter_failed: {error}"),
                                )
                            });
                        }
                        None => {}
                    }
                }
                result = operations.identity_operations.next(), if !operations.identity_operations.is_empty() => {
                    internal_result_burst = internal_result_burst.saturating_add(1);
                    match result {
                        Some(Ok(())) => {}
                        Some(Err(error)) => {
                            self.bus.publish_lazy(|| {
                                Event::runtime_failure(
                                    "controller",
                                    format!("identity_operation_failed: {error}"),
                                )
                            });
                        }
                        None => {}
                    }
                }
                Some(command) = rx.recv() => {
                    internal_result_burst = 0;
                    self.handle_controller_command(command, &mut operations).await;
                }
                }
            }
        })
        .await;
        drop(operations);
        self.finalize_pending_on_shutdown(&mut rx).await;
        self.finalize_slot_state_on_shutdown().await;
        if let Err(panic) = loop_result {
            self.bus.publish_lazy(|| {
                Event::runtime_failure("controller", format!("controller_loop_panicked: {panic}"))
            });
        }
        Ok(())
    }

    /// Applies one command after it leaves the controller command queue.
    async fn handle_controller_command(
        &self,
        command: ControllerCommand,
        operations: &mut TrackedOperations,
    ) {
        match command {
            ControllerCommand::Submit(sub) => {
                let _ = self
                    .guarded(
                        "handle_submission",
                        self.handle_submission(*sub, operations),
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
                        self.handle_identity_operation(id, operation, reply, operations),
                    )
                    .await;
            }
        }
    }

    /// Runs one controller work unit behind a panic boundary.
    ///
    /// A panic is converted into a diagnostic `RuntimeFailure` event and the loop continues.
    pub(in crate::controller::engine) async fn guarded<T>(
        &self,
        who: &'static str,
        fut: impl Future<Output = T>,
    ) -> Option<T> {
        match crate::core::panic_guard::guarded(fut).await {
            Ok(output) => Some(output),
            Err(msg) => {
                self.bus.publish_lazy(|| {
                    Event::runtime_failure("controller", format!("{who}_panicked: {msg}"))
                });
                None
            }
        }
    }
}
