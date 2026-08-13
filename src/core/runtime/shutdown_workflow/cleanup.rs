//! Runs trigger-specific task drain logic and the mandatory cleanup tail.
//!
//! The detached shutdown owner calls this package after the coordinator chooses
//! the first trigger. Explicit and natural shutdown close command admission and
//! require a registry fence before draining tasks within the configured grace
//! period. A signal setup failure closes admission and attempts the same fence,
//! but skips that normal drain.
//!
//! Every trigger then attempts the same cleanup order: join the optional
//! controller, cancel runtime listeners, join the registry listener, join the
//! event relay, and close subscriber callback workers. Later phases still run
//! after a failure or panic.
//! Subscriber cleanup has its own timeout and is not part of the task grace period.

use super::{ShutdownOutcome, ShutdownTrigger};
use crate::{
    core::runtime::SupervisorCore,
    error::RuntimeError,
    events::{Event, EventKind},
};

impl SupervisorCore {
    /// Selects the normal drain, signal-failure, or injected test-panic branch.
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

    /// Produces the trigger outcome and then runs the common cleanup tail.
    ///
    /// A panic in trigger handling is reported and converted to an unclean outcome.
    pub(super) async fn perform_shutdown(&self, trigger: ShutdownTrigger) -> ShutdownOutcome {
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

    /// Attempts every cleanup phase even when an earlier phase fails or panics.
    ///
    /// Returns whether all phases finished cleanly.
    pub(super) async fn finish_shutdown_cleanup(&self) -> bool {
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

    /// Publishes a best-effort diagnostic for a caught shutdown panic.
    pub(super) fn report_shutdown_panic(&self, phase: &str, panic: String) {
        self.bus.publish_lazy(|| {
            Event::runtime_failure("shutdown_owner", format!("{phase} panic: {panic}"))
        });
    }

    /// Fences prior registry commands and spends one grace window on task cleanup.
    ///
    /// `cancel_all_within` spends the shared deadline on newly claimed actors
    /// and removal owners already in progress. The follow-up wait receives only
    /// the unused part. Forced actors and owners still pending form `stuck`.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::GraceExceeded`] when work remains at the deadline.
    /// Returns a shutdown error when registry admission cannot be fenced.
    async fn drain_with_grace(&self) -> Result<(), RuntimeError> {
        self.close_admission_and_fence_registry().await?;
        let grace = self.settings.runtime.grace();
        let started = tokio::time::Instant::now();
        let mut stuck = self.registry.cancel_all_within(grace).await;
        let remaining = grace.saturating_sub(started.elapsed());
        stuck.extend(self.registry.wait_joins_within(remaining).await);
        if stuck.is_empty() {
            self.bus
                .publish_lazy(|| Event::new(EventKind::AllStoppedWithinGrace));
            Ok(())
        } else {
            self.bus
                .publish_lazy(|| Event::new(EventKind::GraceExceeded));
            Err(RuntimeError::GraceExceeded { grace, stuck })
        }
    }
}
