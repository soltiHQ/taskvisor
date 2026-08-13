//! Creates one cancellation-safe shutdown operation shared by every runtime caller.
//!
//! Explicit requests, static-run triggers, and natural registry completion
//! enter [`ShutdownCoordinator`]. The first trigger installs a detached owner.
//! Canceling the initiating future does not cancel that owner. Concurrent and
//! later callers join the same operation and receive its cached result.
//!
//! ```text
//! first trigger ──► shared operation
//!                         ├── admission ──► close and request Registry fence
//!                         ├── requested or natural ──► drain tasks within grace
//!                         └── mandatory cleanup tail ──► cached result
//! ```
//!
//! The cleanup submodule owns the ordered drain and cleanup phases. A signal
//! setup failure skips the normal task drain but still closes admission,
//! attempts the registry fence, and runs the common tail. Dropping the last
//! runtime owner before an operation exists can only close admission and cancel
//! runtime tokens; that synchronous fallback cannot await or report cleanup.

mod cleanup;

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

/// Stores the first shutdown operation and exposes its result to every caller.
pub(super) struct ShutdownCoordinator {
    /// Wakes runtime paths as soon as shutdown admission starts.
    pub(super) started: CancellationToken,
    /// Prevents last-owner fallback from canceling a detached cleanup owner.
    operation_installed: AtomicBool,
    /// Serializes first-trigger ownership and stores the shared operation.
    operation: std::sync::Mutex<Option<Arc<ShutdownOperation>>>,
}

impl ShutdownCoordinator {
    /// Creates a coordinator with no installed operation.
    pub(super) fn new() -> Self {
        Self {
            started: CancellationToken::new(),
            operation_installed: AtomicBool::new(false),
            operation: std::sync::Mutex::new(None),
        }
    }
}

/// Gives each waiter a receiver for one detached owner's cached outcome.
struct ShutdownOperation {
    /// Watch channel retained for current and later callers.
    outcome: watch::Receiver<Option<ShutdownOutcome>>,
}

impl ShutdownOperation {
    /// Waits for the cached result and maps unexpected owner loss to shutdown failure.
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

/// Retains a custom I/O error source chain across cloned shutdown outcomes.
#[derive(Debug)]
struct SharedIoError(
    /// Original source shared by every converted caller error.
    Arc<std::io::Error>,
);

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

/// Cached shutdown result that can create one owned public error per caller.
#[derive(Clone)]
enum ShutdownOutcome {
    /// Task drain and the common cleanup tail completed.
    Completed,
    /// The task grace window ended with pending work.
    GraceExceeded {
        /// Configured task cleanup deadline.
        grace: Duration,
        /// Task labels and join reporters pending at the deadline.
        stuck: Vec<Arc<str>>,
    },
    /// Operating-system signal setup failed before normal task drain.
    SignalSetupFailed {
        /// Signal setup source retained for every caller.
        source: Arc<std::io::Error>,
    },
    /// Internal shutdown ownership or cleanup failed without a more specific result.
    ShuttingDown,
}

impl ShutdownOutcome {
    /// Reduces normal task-drain results to the cacheable outcome set.
    fn from_drain_result(result: Result<(), RuntimeError>) -> Self {
        match result {
            Ok(()) => Self::Completed,
            Err(RuntimeError::GraceExceeded { grace, stuck }) => {
                Self::GraceExceeded { grace, stuck }
            }
            Err(_) => Self::ShuttingDown,
        }
    }

    /// Rebuilds one caller-owned runtime result from the cached outcome.
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

/// First trigger that selects the shared shutdown branch.
pub(super) enum ShutdownTrigger {
    /// A handle or application trigger requested graceful shutdown.
    Requested,
    /// The registry became empty without an explicit request.
    Natural,
    /// Operating-system signal setup failed.
    SignalSetupFailed(
        /// Source preserved for all shutdown callers.
        Arc<std::io::Error>,
    ),
    /// Forces the detached-owner panic path in runtime tests.
    #[cfg(test)]
    PanicForTest,
}

impl SupervisorCore {
    /// Applies the synchronous last-owner fallback when no cleanup owner exists.
    ///
    /// An installed detached operation retains cleanup ownership.
    /// Otherwise, this closes admission and cancels the shutdown-start and runtime tokens.
    pub(crate) fn abandon(&self) {
        if self.shutdown.operation_installed.load(Ordering::Acquire) {
            return;
        }

        self.mark_shutting_down();
        self.shutdown.started.cancel();
        self.runtime_token.cancel();
    }

    /// Returns the installed operation or installs the first trigger's detached owner.
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
            self.bus
                .publish_lazy(|| Event::new(EventKind::ShutdownRequested));
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

    /// Starts or joins shutdown and converts its cached outcome for this caller.
    ///
    /// # Errors
    ///
    /// Returns the cached error when grace is exceeded, signal setup fails, or
    /// internal cleanup cannot finish cleanly.
    pub(super) async fn join_shutdown(
        self: &Arc<Self>,
        trigger: ShutdownTrigger,
    ) -> Result<(), RuntimeError> {
        self.begin_shutdown(trigger).wait().await.into_result()
    }

    /// Joins an operation after its reliable shutdown-start signal fires.
    ///
    /// # Errors
    ///
    /// Returns the cached shutdown error from the shared operation.
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

    /// Enters the shared workflow with an explicit graceful-shutdown trigger.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::SignalSetupFailed`] when this call joins an
    /// operation started by failed operating-system signal setup.
    /// Returns [`RuntimeError::GraceExceeded`] when tasks remain after the grace window.
    /// Returns [`RuntimeError::ShuttingDown`] when internal cleanup cannot finish cleanly.
    pub(crate) async fn shutdown(self: &Arc<Self>) -> Result<(), RuntimeError> {
        self.join_shutdown(ShutdownTrigger::Requested).await
    }
}
