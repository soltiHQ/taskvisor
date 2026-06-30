//! # Public supervisor facade.
//!
//! [`Supervisor`] is the public entry point for running taskvisor.
//!
//! It owns the runtime core and, when the `controller` feature is enabled, an optional controller.
//! The runtime implementation lives in [`SupervisorCore`]; this type keeps the public API small and stable.
//!
//! ## Modes
//!
//! - [`run`](Supervisor::run): run a fixed set of tasks until natural completion or an OS shutdown signal.
//! - [`serve`](Supervisor::serve): start the runtime and return a [`SupervisorHandle`](crate::SupervisorHandle) for dynamic task management.
//!
//! [`SupervisorCore`]: crate::core::SupervisorCore

use std::sync::Arc;

use crate::core::{SupervisorConfig, SupervisorCore, builder::SupervisorBuilder};
use crate::{error::RuntimeError, subscribers::Subscribe, tasks::TaskSpec};

/// Public facade for the taskvisor runtime.
///
/// Use [`new`](Self::new) for simple construction, or [`builder`](Self::builder) when you need to configure optional features.
///
/// ## Static Mode
///
/// Static mode runs a known task set:
///
/// ```rust,no_run
/// # use taskvisor::prelude::*;
/// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
/// supervisor.run(vec![/* task specs */]).await?;
/// # Ok(()) }
/// ```
///
/// ## Dynamic Mode
///
/// Dynamic mode returns a handle for runtime task management:
///
/// ```rust,no_run
/// # use taskvisor::prelude::*;
/// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
/// let handle = supervisor.serve();
///
/// // handle.add(...)?;
/// // handle.cancel(id).await?;
/// // handle.shutdown().await?;
/// # Ok(()) }
/// ```
///
/// # Also
///
/// - [`SupervisorHandle`](crate::SupervisorHandle) - dynamic runtime management API
/// - [`SupervisorBuilder`](crate::SupervisorBuilder) - step-by-step construction
/// - [`SupervisorConfig`] - runtime defaults and limits
pub struct Supervisor {
    core: Arc<SupervisorCore>,

    #[cfg(feature = "controller")]
    controller: Option<Arc<crate::controller::Controller>>,
    #[cfg(feature = "controller")]
    runtime_token: tokio_util::sync::CancellationToken,
    #[cfg(feature = "controller")]
    controller_started: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor")
            .field("core", &self.core)
            .finish_non_exhaustive()
    }
}

impl Supervisor {
    /// Creates a supervisor from already-built runtime parts.
    pub(super) fn from_parts(
        core: Arc<SupervisorCore>,
        #[cfg(feature = "controller")] controller: Option<Arc<crate::controller::Controller>>,
        #[cfg(feature = "controller")] runtime_token: tokio_util::sync::CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            core,
            #[cfg(feature = "controller")]
            controller,
            #[cfg(feature = "controller")]
            runtime_token,
            #[cfg(feature = "controller")]
            controller_started: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Starts the controller loop once, if a controller is configured.
    #[cfg(feature = "controller")]
    fn start_controller(&self) {
        use std::sync::atomic::Ordering;
        if let Some(ctrl) = &self.controller
            && !self.controller_started.swap(true, Ordering::AcqRel)
        {
            ctrl.clone().run(self.runtime_token.clone());
        }
    }

    /// Creates a new supervisor with config and subscribers.
    ///
    /// The returned supervisor is not started yet.
    /// Call [`run`](Self::run) for a fixed task set or [`serve`](Self::serve) for dynamic management.
    pub fn new(cfg: SupervisorConfig, subscribers: Vec<Arc<dyn Subscribe>>) -> Arc<Self> {
        Self::builder(cfg).with_subscribers(subscribers).build()
    }

    /// Creates a builder for constructing a supervisor.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use taskvisor::{Supervisor, SupervisorConfig};
    ///
    /// let supervisor = Supervisor::builder(SupervisorConfig::default())
    ///     .with_subscribers(vec![])
    ///     .build();
    /// ```
    pub fn builder(cfg: SupervisorConfig) -> SupervisorBuilder {
        SupervisorBuilder::new(cfg)
    }

    /// Starts the runtime and returns a handle for dynamic task management.
    ///
    /// Safe to call more than once.
    /// Runtime listeners are started once, and each call returns a handle to the same runtime.
    pub fn serve(self: &Arc<Self>) -> super::handle::SupervisorHandle {
        self.core.start();
        #[cfg(feature = "controller")]
        self.start_controller();
        let handle = super::handle::SupervisorHandle::new(Arc::clone(&self.core));
        #[cfg(feature = "controller")]
        let handle = handle.with_controller(self.controller.clone());
        handle
    }

    /// Runs a fixed task set until natural completion or OS shutdown signal.
    ///
    /// This is static mode.
    /// It starts the runtime, submits the provided tasks, and waits until all tasks complete or a shutdown signal is received.
    ///
    /// `run` is single-shot for one supervisor instance.
    /// A second call returns [`RuntimeError::AlreadyRunning`].
    pub async fn run(&self, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
        #[cfg(feature = "controller")]
        self.start_controller();
        self.core.run(tasks).await
    }

    /// Returns the runtime core for controller tests.
    #[cfg(all(test, feature = "controller"))]
    pub(crate) fn core(&self) -> &Arc<SupervisorCore> {
        &self.core
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TaskContext, TaskFn, TaskRef};
    use std::time::Duration;

    #[tokio::test]
    async fn shutdown_force_terminates_noncooperative_task_within_grace() {
        let cfg = SupervisorConfig {
            grace: Duration::from_millis(200),
            ..Default::default()
        };
        let sup = Supervisor::new(cfg, vec![]);
        let handle = sup.serve();

        let stubborn: TaskRef = TaskFn::arc("stubborn", |_ctx: TaskContext| async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        });
        handle
            .add_and_wait(TaskSpec::once(stubborn), Duration::from_secs(1))
            .await
            .expect("task should register");

        let res = tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown must return within grace, not block on the stuck task");

        match res {
            Err(RuntimeError::GraceExceeded { stuck, .. }) => {
                assert!(
                    stuck.iter().any(|n| &**n == "stubborn"),
                    "stuck set must list the non-cooperative task, got {stuck:?}"
                );
            }
            other => panic!("expected GraceExceeded listing the stuck task, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shutdown_cooperative_task_returns_ok() {
        let cfg = SupervisorConfig {
            grace: Duration::from_secs(5),
            ..Default::default()
        };
        let sup = Supervisor::new(cfg, vec![]);
        let handle = sup.serve();

        let good: TaskRef = TaskFn::arc("good", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        handle
            .add_and_wait(TaskSpec::restartable(good), Duration::from_secs(1))
            .await
            .expect("task should register");

        let res = tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("cooperative shutdown must not hang");
        assert!(
            res.is_ok(),
            "cooperative shutdown should be Ok, got {res:?}"
        );
    }

    #[tokio::test]
    async fn add_and_wait_duplicate_name_returns_already_exists() {
        let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
        let handle = sup.serve();

        let make = || -> TaskRef {
            TaskFn::arc("dup", |ctx: TaskContext| async move {
                ctx.cancelled().await;
                Ok(())
            })
        };
        handle
            .add_and_wait(TaskSpec::restartable(make()), Duration::from_secs(1))
            .await
            .expect("first add should succeed");

        let res = handle
            .add_and_wait(TaskSpec::restartable(make()), Duration::from_secs(1))
            .await;
        assert!(
            matches!(res, Err(RuntimeError::TaskAlreadyExists { .. })),
            "duplicate add must return TaskAlreadyExists, got {res:?}"
        );

        let _ = handle.shutdown().await;
    }

    #[tokio::test]
    async fn cancel_reports_removed_then_absent() {
        let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
        let handle = sup.serve();

        let t: TaskRef = TaskFn::arc("c", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        let id = handle
            .add_and_wait(TaskSpec::restartable(t), Duration::from_secs(1))
            .await
            .expect("add should succeed");

        let removed = handle.cancel(id).await.expect("cancel should not error");
        assert!(
            removed,
            "cancelling an existing task should report removed=true"
        );

        let absent = handle.cancel(id).await.expect("cancel should not error");
        assert!(!absent, "cancelling a missing task should report false");

        let _ = handle.shutdown().await;
    }
}
