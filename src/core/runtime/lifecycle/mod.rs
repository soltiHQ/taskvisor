//! Starts the workers that make a wired supervisor operational.
//!
//! [`Supervisor::serve`](crate::Supervisor::serve) and the static run methods enter this package
//! after the builder has connected the core components. Startup is idempotent after success and
//! serialized while it is in progress.
//!
//! ```text
//! builder-wired core
//!         ▼
//!       start
//!         ├── subscribers ──► callback executor
//!         ├── enabled bus ──► event relay
//!         ├── registry ─────► listener and reaper
//!         └── controller ───► optional worker
//! ```
//!
//! The `static_run` submodule adds the single-use lifecycle used by `run`, `run_until`, and `run_with_os_signals`.

mod static_run;

use std::sync::atomic::Ordering;

use super::SupervisorCore;
use crate::error::RuntimeError;

impl SupervisorCore {
    /// Makes startup visible only after every configured worker is launched.
    ///
    /// Repeated calls after successful startup return without launching more workers.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::TokioRuntimeUnavailable`] outside a Tokio runtime.
    /// Returns [`RuntimeError::ThreadStartFailed`] when subscriber workers cannot start.
    pub(crate) fn start(&self) -> Result<(), RuntimeError> {
        if self.started.load(Ordering::Acquire) {
            return Ok(());
        }

        let _startup = self
            .startup_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.started.load(Ordering::Acquire) {
            return Ok(());
        }

        tokio::runtime::Handle::try_current().map_err(|_| RuntimeError::TokioRuntimeUnavailable)?;
        self.subs.start()?;
        if self.bus.is_enabled() {
            self.subscriber_listener();
        }
        self.registry.clone().spawn_listener();
        #[cfg(feature = "controller")]
        if let Some(controller) = self.controller.get().and_then(std::sync::Weak::upgrade) {
            controller.run();
        }
        self.started.store(true, Ordering::Release);
        Ok(())
    }
}
