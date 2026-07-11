//! # Supervisor builder.
//!
//! Builds a [`Supervisor`](crate::Supervisor) from [`SupervisorConfig`] and optional runtime extensions.
//!
//! ## Example
//!
//! ```rust
//! use std::time::Duration;
//! use taskvisor::{BackoffPolicy, SupervisorBuilder, SupervisorConfig};
//!
//! let supervisor = SupervisorBuilder::new(SupervisorConfig::default())
//!     .with_grace(Duration::from_secs(30))
//!     .with_subscriber_shutdown_timeout(Duration::from_secs(5))
//!     .with_timeout(Duration::from_secs(5))
//!     .with_max_retries(10)
//!     .with_max_concurrent(4)
//!     .with_backoff(BackoffPolicy::exponential(Duration::from_millis(100)))
//!     .build();
//! ```

use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

use tokio::{sync, sync::mpsc};

use super::{
    alive::AliveTracker, registry::Registry, runtime::SupervisorCore, supervisor::Supervisor,
};
use crate::{
    core::SupervisorConfig,
    events::Bus,
    policies::{BackoffPolicy, RestartPolicy},
    subscribers::{DEFAULT_SHUTDOWN_TIMEOUT, Subscribe, SubscriberSet},
};

/// Builder for constructing a [`Supervisor`](crate::Supervisor).
///
/// Use this when you need to customize runtime config, subscribers, or optional feature-backed components before starting the supervisor.
///
/// The produced supervisor is not started yet.
/// Calling `build` only creates the runtime graph and stores the pieces that will be used later by `Supervisor::run` or `Supervisor::serve`.
///
/// # Also
///
/// - [`Supervisor`](crate::Supervisor) - runtime facade produced by this builder
/// - [`SupervisorConfig`] - runtime defaults and limits
/// - [`Subscribe`](crate::Subscribe) - event subscriber trait
pub struct SupervisorBuilder {
    cfg: SupervisorConfig,
    subscribers: Vec<Arc<dyn Subscribe>>,
    subscriber_shutdown_timeout: Duration,

    #[cfg(feature = "controller")]
    controller_config: Option<crate::controller::ControllerConfig>,
}

impl SupervisorBuilder {
    /// Creates a new builder with the given runtime configuration.
    ///
    /// Start from [`SupervisorConfig::default`] and adjust single values with the `with_*` setters below.
    pub fn new(cfg: SupervisorConfig) -> Self {
        Self {
            cfg,
            subscribers: Vec::new(),
            subscriber_shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,

            #[cfg(feature = "controller")]
            controller_config: None,
        }
    }

    /// Builder: sets the graceful-shutdown wait time.
    ///
    /// During shutdown the supervisor waits up to `grace` for tasks to stop.
    /// `Duration::ZERO` means no graceful wait.
    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.cfg.grace = grace;
        self
    }

    /// Sets the shared timeout for draining subscriber queues during shutdown.
    ///
    /// The default is five seconds.
    /// `Duration::ZERO` closes the queues and aborts their async workers without waiting for queued events.
    /// A callback already running on Tokio's blocking pool cannot be aborted and may finish later.
    /// Tokio runtime shutdown may still wait for that running blocking callback.
    /// Reaching this best-effort cleanup timeout does not produce a [`RuntimeError`](crate::RuntimeError).
    pub fn with_subscriber_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.subscriber_shutdown_timeout = timeout;
        self
    }

    /// Builder: sets the default timeout for one task attempt.
    ///
    /// A zero duration means no timeout.
    /// Tasks created outside [`SupervisorConfig::task_spec`] keep their own timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.cfg.timeout = Some(timeout).filter(|d| !d.is_zero());
        self
    }

    /// Builder: sets the default failure-retry limit.
    ///
    /// The default is unlimited retries.
    /// Call this only when you want a limit.
    ///
    /// Use [`RestartPolicy::Never`] to stop a task after its first failure.
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        assert!(
            max_retries > 0,
            "with_max_retries(0) is invalid: zero retries cannot be represented; \
             use RestartPolicy::Never to stop after the first failure"
        );
        self.cfg.max_retries = NonZeroU32::new(max_retries);
        self
    }

    /// Builder: sets the global limit for concurrently running task attempts.
    ///
    /// The default is unlimited concurrency.
    /// Call this only when you want a limit.
    ///
    /// With zero permits no task could ever run.
    pub fn with_max_concurrent(mut self, max_concurrent: usize) -> Self {
        assert!(
            max_concurrent > 0,
            "with_max_concurrent(0) is invalid: no task could ever run; \
             omit the call for unlimited concurrency"
        );
        self.cfg.max_concurrent = NonZeroUsize::new(max_concurrent);
        self
    }

    /// Builder: sets the event bus capacity.
    ///
    /// The effective capacity is at least `1`.
    /// Slow subscribers that fall behind by more than this may skip older events.
    pub fn with_bus_capacity(mut self, bus_capacity: usize) -> Self {
        self.cfg.bus_capacity = bus_capacity;
        self
    }

    /// Builder: sets the registry management command queue capacity.
    ///
    /// The effective capacity is at least `1`.
    /// Sync management methods fail fast when the queue is full.
    pub fn with_registry_queue_capacity(mut self, registry_queue_capacity: usize) -> Self {
        self.cfg.registry_queue_capacity = registry_queue_capacity;
        self
    }

    /// Builder: sets the default restart policy for tasks created through [`SupervisorConfig::task_spec`].
    ///
    /// Restart controls whether a task runs again after it exits.
    pub fn with_restart(mut self, restart: RestartPolicy) -> Self {
        self.cfg.restart = restart;
        self
    }

    /// Builder: sets the default backoff policy for retryable failures.
    ///
    /// Backoff controls the delay before a failed attempt restarts.
    pub fn with_backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.cfg.backoff = backoff;
        self
    }

    /// Sets event subscribers for runtime observability.
    ///
    /// Each subscriber gets its own bounded queue and worker task.
    /// The workers are created during [`build`](Self::build), and events are delivered after the supervisor starts its runtime listener.
    ///
    /// Passing a new list replaces any subscribers previously set on this builder.
    pub fn with_subscribers(mut self, subscribers: Vec<Arc<dyn Subscribe>>) -> Self {
        self.subscribers = subscribers;
        self
    }

    /// Enables the controller with the given configuration.
    ///
    /// The controller adds slot-based task admission on top of the core supervisor.
    /// It can queue, replace, or drop submissions depending on the configured slot policy.
    ///
    /// Available only with the `controller` feature.
    #[cfg(feature = "controller")]
    pub fn with_controller(mut self, config: crate::controller::ControllerConfig) -> Self {
        self.controller_config = Some(config);
        self
    }

    /// Builds the supervisor.
    ///
    /// This consumes the builder and creates all runtime components.
    ///
    /// The supervisor is returned in a stopped state.
    /// Task execution starts when the caller runs or serves the supervisor.
    pub fn build(self) -> Arc<Supervisor> {
        let bus = Bus::new(self.cfg.bus_capacity_clamped());
        let subs = Arc::new(SubscriberSet::new_with_shutdown_timeout(
            self.subscribers,
            bus.clone(),
            self.subscriber_shutdown_timeout,
        ));
        let runtime_token = tokio_util::sync::CancellationToken::new();

        let semaphore = self
            .cfg
            .max_concurrent
            .map(|n| Arc::new(sync::Semaphore::new(n.get())));

        let (cmd_tx, cmd_rx) = mpsc::channel(self.cfg.registry_queue_capacity_clamped());
        let registry = Registry::new(
            bus.clone(),
            runtime_token.clone(),
            semaphore,
            self.cfg.grace,
            cmd_rx,
        );
        let alive = Arc::new(AliveTracker::new());

        let core = SupervisorCore::new_internal(
            self.cfg,
            bus.clone(),
            subs,
            alive,
            registry,
            runtime_token.clone(),
            cmd_tx,
        );

        #[cfg(feature = "controller")]
        let controller = self
            .controller_config
            .map(|ctrl_cfg| crate::controller::Controller::new(ctrl_cfg, &core, bus.clone()));

        Supervisor::from_parts(
            core,
            #[cfg(feature = "controller")]
            controller,
            #[cfg(feature = "controller")]
            runtime_token,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policies::{BackoffPolicy, RestartPolicy};
    use std::time::Duration;

    #[test]
    fn default_subscriber_shutdown_timeout_is_five_seconds() {
        let builder = SupervisorBuilder::new(SupervisorConfig::default());
        assert_eq!(builder.subscriber_shutdown_timeout, Duration::from_secs(5));
    }

    #[test]
    fn setters_override_config_fields() {
        let backoff = BackoffPolicy::exponential(Duration::from_millis(200));
        let b = SupervisorBuilder::new(SupervisorConfig::default())
            .with_grace(Duration::from_secs(30))
            .with_subscriber_shutdown_timeout(Duration::from_secs(2))
            .with_timeout(Duration::from_secs(5))
            .with_max_retries(10)
            .with_max_concurrent(4)
            .with_bus_capacity(2048)
            .with_registry_queue_capacity(256)
            .with_restart(RestartPolicy::Never)
            .with_backoff(backoff);

        assert_eq!(b.cfg.grace, Duration::from_secs(30));
        assert_eq!(b.subscriber_shutdown_timeout, Duration::from_secs(2));
        assert_eq!(b.cfg.timeout, Some(Duration::from_secs(5)));
        assert_eq!(b.cfg.max_retries.map(|n| n.get()), Some(10));
        assert_eq!(b.cfg.max_concurrent.map(|n| n.get()), Some(4));
        assert_eq!(b.cfg.bus_capacity, 2048);
        assert_eq!(b.cfg.registry_queue_capacity, 256);
        assert!(
            matches!(b.cfg.restart, RestartPolicy::Never),
            "with_restart must store the given policy"
        );
        assert_eq!(
            b.cfg.backoff.factor(),
            2.0,
            "with_backoff must store the given policy"
        );
    }

    #[test]
    fn with_timeout_zero_means_no_timeout() {
        let b = SupervisorBuilder::new(SupervisorConfig {
            timeout: Some(Duration::from_secs(5)),
            ..Default::default()
        })
        .with_timeout(Duration::ZERO);

        assert_eq!(
            b.cfg.timeout, None,
            "a zero timeout must normalize to None (no timeout)"
        );
    }

    #[test]
    #[should_panic(expected = "with_max_retries(0)")]
    fn with_max_retries_zero_panics() {
        let _ = SupervisorBuilder::new(SupervisorConfig::default()).with_max_retries(0);
    }

    #[test]
    #[should_panic(expected = "with_max_concurrent(0)")]
    fn with_max_concurrent_zero_panics() {
        let _ = SupervisorBuilder::new(SupervisorConfig::default()).with_max_concurrent(0);
    }
}
