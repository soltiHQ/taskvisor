//! Cancellation-safe shared shutdown owner and ordered cleanup workflow.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::SupervisorCore;
use crate::{
    error::RuntimeError,
    events::{Event, EventKind},
};

/// Coordinates one cancellation-safe shutdown operation for every caller.
pub(super) struct ShutdownCoordinator {
    pub(super) started: CancellationToken,
    operation_installed: AtomicBool,
    operation: std::sync::Mutex<Option<Arc<ShutdownOperation>>>,
}

impl ShutdownCoordinator {
    pub(super) fn new() -> Self {
        Self {
            started: CancellationToken::new(),
            operation_installed: AtomicBool::new(false),
            operation: std::sync::Mutex::new(None),
        }
    }
}

/// Shared, cached result of one detached shutdown owner.
struct ShutdownOperation {
    outcome: watch::Receiver<Option<ShutdownOutcome>>,
}

impl ShutdownOperation {
    async fn wait(&self) -> ShutdownOutcome {
        let mut outcome = self.outcome.clone();
        loop {
            if let Some(outcome) = outcome.borrow_and_update().clone() {
                return outcome;
            }
            if outcome.changed().await.is_err() {
                return ShutdownOutcome::ShuttingDown;
            }
        }
    }
}

/// Keeps a custom I/O error and its source chain alive for repeated delivery.
#[derive(Debug)]
struct SharedIoError(Arc<std::io::Error>);

impl std::fmt::Display for SharedIoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self.0.as_ref(), f)
    }
}

impl std::error::Error for SharedIoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        if let Some(source) = self.0.get_ref() {
            Some(source)
        } else {
            Some(self.0.as_ref())
        }
    }
}

/// Cloneable internal result materialized into one owned public error per caller.
#[derive(Clone)]
enum ShutdownOutcome {
    Completed,
    GraceExceeded {
        grace: Duration,
        stuck: Vec<Arc<str>>,
    },
    SignalSetupFailed {
        source: Arc<std::io::Error>,
    },
    ShuttingDown,
}

impl ShutdownOutcome {
    fn from_drain_result(result: Result<(), RuntimeError>) -> Self {
        match result {
            Ok(()) => Self::Completed,
            Err(RuntimeError::GraceExceeded { grace, stuck }) => {
                Self::GraceExceeded { grace, stuck }
            }
            Err(_) => Self::ShuttingDown,
        }
    }

    fn into_result(self) -> Result<(), RuntimeError> {
        match self {
            Self::Completed => Ok(()),
            Self::GraceExceeded { grace, stuck } => {
                Err(RuntimeError::GraceExceeded { grace, stuck })
            }
            Self::SignalSetupFailed { source } => {
                let source = if let Some(code) = source.raw_os_error() {
                    std::io::Error::from_raw_os_error(code)
                } else {
                    std::io::Error::new(source.kind(), SharedIoError(source))
                };
                Err(RuntimeError::SignalSetupFailed { source })
            }
            Self::ShuttingDown => Err(RuntimeError::ShuttingDown),
        }
    }
}

/// Cause that wins ownership of the shared shutdown operation.
pub(super) enum ShutdownTrigger {
    Requested,
    Natural,
    SignalSetupFailed(Arc<std::io::Error>),
    #[cfg(test)]
    PanicForTest,
}

impl SupervisorCore {
    /// Performs the non-blocking last-owner safety shutdown.
    ///
    /// This path cannot await graceful cleanup or report its result. When no
    /// explicit shutdown operation exists, it closes command admission, stops
    /// controller intake, and cancels runtime listeners. An already-started
    /// detached graceful shutdown keeps ownership of cleanup and is not
    /// overridden by this fallback.
    pub(crate) fn abandon(&self) {
        if self.shutdown.operation_installed.load(Ordering::Acquire) {
            return;
        }

        self.mark_shutting_down();
        self.shutdown.started.cancel();
        self.runtime_token.cancel();
    }

    /// Returns the shared operation, starting one detached shutdown owner if needed.
    fn begin_shutdown(self: &Arc<Self>, trigger: ShutdownTrigger) -> Arc<ShutdownOperation> {
        let mut operation = self
            .shutdown
            .operation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(operation) = operation.as_ref() {
            return Arc::clone(operation);
        }

        let (outcome_tx, outcome_rx) = watch::channel(None);
        let shared = Arc::new(ShutdownOperation {
            outcome: outcome_rx,
        });

        self.mark_shutting_down();
        *operation = Some(Arc::clone(&shared));
        self.shutdown
            .operation_installed
            .store(true, Ordering::Release);
        if matches!(&trigger, ShutdownTrigger::Requested) {
            self.bus.publish(Event::new(EventKind::ShutdownRequested));
        }
        self.shutdown.started.cancel();
        drop(operation);

        let core = Arc::clone(self);
        tokio::spawn(async move {
            let outcome =
                match crate::core::panic_guard::guarded(core.perform_shutdown(trigger)).await {
                    Ok(outcome) => outcome,
                    Err(panic) => {
                        core.report_shutdown_panic("owner", panic);
                        let _ = core.finish_shutdown_cleanup().await;
                        ShutdownOutcome::ShuttingDown
                    }
                };
            outcome_tx.send_replace(Some(outcome));
        });

        shared
    }

    /// Starts or joins the shared shutdown operation.
    pub(super) async fn join_shutdown(
        self: &Arc<Self>,
        trigger: ShutdownTrigger,
    ) -> Result<(), RuntimeError> {
        self.begin_shutdown(trigger).wait().await.into_result()
    }

    /// Waits for an operation already started by another runtime entry point.
    pub(super) async fn wait_started_shutdown(&self) -> Result<(), RuntimeError> {
        self.shutdown.started.cancelled().await;
        let operation = self
            .shutdown
            .operation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .cloned()
            .expect("started shutdown must publish its shared operation");
        operation.wait().await.into_result()
    }

    /// Initiates explicit graceful shutdown.
    ///
    /// Starts or joins one cancellation-safe shutdown operation.
    pub(crate) async fn shutdown(self: &Arc<Self>) -> Result<(), RuntimeError> {
        self.join_shutdown(ShutdownTrigger::Requested).await
    }

    /// Resolves the trigger-specific part of shutdown before common cleanup.
    async fn resolve_shutdown(&self, trigger: ShutdownTrigger) -> ShutdownOutcome {
        match trigger {
            ShutdownTrigger::Requested => {
                ShutdownOutcome::from_drain_result(self.drain_with_grace().await)
            }
            ShutdownTrigger::Natural => {
                ShutdownOutcome::from_drain_result(self.drain_with_grace().await)
            }
            ShutdownTrigger::SignalSetupFailed(source) => {
                let _ = self.close_admission_and_fence_registry().await;
                ShutdownOutcome::SignalSetupFailed { source }
            }
            #[cfg(test)]
            ShutdownTrigger::PanicForTest => panic!("injected shutdown panic"),
        }
    }

    /// Owns trigger handling and the mandatory cleanup tail.
    async fn perform_shutdown(&self, trigger: ShutdownTrigger) -> ShutdownOutcome {
        let outcome = match crate::core::panic_guard::guarded(self.resolve_shutdown(trigger)).await
        {
            Ok(outcome) => outcome,
            Err(panic) => {
                self.report_shutdown_panic("drain", panic);
                ShutdownOutcome::ShuttingDown
            }
        };

        if self.finish_shutdown_cleanup().await {
            outcome
        } else {
            ShutdownOutcome::ShuttingDown
        }
    }

    /// Cancels runtime listeners and closes subscribers, attempting every phase.
    async fn finish_shutdown_cleanup(&self) -> bool {
        let mut clean = true;

        #[cfg(feature = "controller")]
        if let Some(controller) = self.controller.get().and_then(std::sync::Weak::upgrade) {
            match crate::core::panic_guard::guarded(controller.join()).await {
                Ok(true) => {}
                Ok(false) => clean = false,
                Err(panic) => {
                    self.report_shutdown_panic("controller cleanup", panic);
                    clean = false;
                }
            }
        }

        self.runtime_token.cancel();

        match crate::core::panic_guard::guarded(self.registry.join_listener()).await {
            Ok(true) => {}
            Ok(false) => clean = false,
            Err(panic) => {
                self.report_shutdown_panic("registry cleanup", panic);
                clean = false;
            }
        }
        match crate::core::panic_guard::guarded(self.join_subscriber_listener()).await {
            Ok(true) => {}
            Ok(false) => clean = false,
            Err(panic) => {
                self.report_shutdown_panic("subscriber listener cleanup", panic);
                clean = false;
            }
        }
        if let Err(panic) = crate::core::panic_guard::guarded(self.subs.close()).await {
            self.report_shutdown_panic("subscriber worker cleanup", panic);
            clean = false;
        }

        clean
    }

    /// Reports an internal shutdown panic without interrupting later cleanup phases.
    fn report_shutdown_panic(&self, phase: &str, panic: String) {
        self.bus.publish(Event::subscriber_panicked(
            "shutdown_owner",
            format!("{phase} panic: {panic}"),
        ));
    }

    /// Cancels tasks and waits for them within the configured grace window.
    ///
    /// Admission closes first. The registry processes every command accepted
    /// before that point, then this drains registered tasks and waits for
    /// detached join reporters using the remaining grace.
    /// Tasks/joiners that do not finish are returned as `GraceExceeded` stuck labels.
    async fn drain_with_grace(&self) -> Result<(), RuntimeError> {
        self.close_admission_and_fence_registry().await?;
        let grace = self.settings.runtime.grace();
        let effective_grace = grace.min(Duration::from_secs(60 * 60 * 24 * 365 * 30));
        let started = tokio::time::Instant::now();
        let mut stuck = self.registry.cancel_all_within(effective_grace).await;
        let remaining = effective_grace.saturating_sub(started.elapsed());
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
