//! # Supervisor: public facade over the runtime ([`SupervisorCore`]) and optional controller.
//!
//! [`Supervisor`] is a thin composition root:
//! it owns an `Arc<SupervisorCore>` and, when the `controller` feature is enabled, an `Arc<Controller>`.
//!
//! The controller depends only on [`SupervisorCore`] (via a `Weak`).
//! The cycle that previously coupled the two is gone: see [`SupervisorCore`].
//!
//! ## Two usage modes
//! - **Static**: [`run`](Supervisor::run) a known set of tasks until completion or Ctrl+C.
//! - **Dynamic**: [`serve`](Supervisor::serve) returns a [`SupervisorHandle`] to add/remove/cancel/submit at runtime.
//!
//! [`SupervisorCore`]: crate::core::SupervisorCore

use std::sync::Arc;

use crate::core::{SupervisorConfig, SupervisorCore, builder::SupervisorBuilder};
use crate::{error::RuntimeError, subscribers::Subscribe, tasks::TaskSpec};

/// Orchestrates task actors, event delivery, and graceful shutdown.
///
/// ## Two usage modes
///
/// **Static**: run a known set of tasks until completion or Ctrl+C:
/// ```rust,no_run
/// # use taskvisor::prelude::*;
/// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
/// sup.run(vec![/* specs */]).await?;
/// # Ok(()) }
/// ```
///
/// **Dynamic**: add/remove/cancel tasks at runtime via [`SupervisorHandle`]:
/// ```rust,no_run
/// # use taskvisor::prelude::*;
/// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
/// let handle = sup.serve();
/// // handle.add(...), handle.cancel(...), handle.shutdown().await
/// # Ok(()) }
/// ```
///
/// # Also
///
/// - [`SupervisorHandle`](crate::SupervisorHandle) - runtime management API returned by [`serve`](Self::serve)
/// - [`SupervisorBuilder`](crate::SupervisorBuilder) - step-by-step construction
/// - [`SupervisorConfig`] - configuration knobs
///
/// [`SupervisorHandle`]: crate::SupervisorHandle
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
    /// Internal facade constructor used by the builder.
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

    /// Starts the controller's event loop exactly once.
    #[cfg(feature = "controller")]
    fn start_controller(&self) {
        use std::sync::atomic::Ordering;
        if let Some(ctrl) = &self.controller
            && !self.controller_started.swap(true, Ordering::AcqRel)
        {
            ctrl.clone().run(self.runtime_token.clone());
        }
    }

    /// Creates a new supervisor with the given config and subscribers (maybe empty).
    pub fn new(cfg: SupervisorConfig, subscribers: Vec<Arc<dyn Subscribe>>) -> Arc<Self> {
        Self::builder(cfg).with_subscribers(subscribers).build()
    }

    /// Creates a builder for constructing a Supervisor.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use taskvisor::{SupervisorConfig, Supervisor};
    ///
    /// let sup = Supervisor::builder(SupervisorConfig::default())
    ///     .with_subscribers(vec![])
    ///     .build();
    /// ```
    pub fn builder(cfg: SupervisorConfig) -> SupervisorBuilder {
        SupervisorBuilder::new(cfg)
    }

    /// Returns a [`SupervisorHandle`](crate::SupervisorHandle) for dynamic task management.
    pub fn serve(self: &Arc<Self>) -> super::handle::SupervisorHandle {
        self.core.start();
        #[cfg(feature = "controller")]
        self.start_controller();
        let handle = super::handle::SupervisorHandle::new(Arc::clone(&self.core));
        #[cfg(feature = "controller")]
        let handle = handle.with_controller(self.controller.clone());
        handle
    }

    /// Runs task specifications until completion or shutdown signal (static mode).
    pub async fn run(&self, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
        #[cfg(feature = "controller")]
        self.start_controller();
        self.core.run(tasks).await
    }

    /// Test-only access to the runtime, for constructing a standalone controller against a live core.
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
