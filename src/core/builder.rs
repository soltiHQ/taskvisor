//! Build a stopped [`Supervisor`].
//!
//! [`SupervisorBuilder::build`] only creates the runtime state and channels.
//! It does not spawn Tokio tasks.
//!
//! [`Supervisor::run`](crate::Supervisor::run) or [`Supervisor::serve`](crate::Supervisor::serve) starts the runtime.
//!
//! ```rust
//! use std::num::NonZeroUsize;
//! use std::time::Duration;
//! use taskvisor::{SupervisorBuilder, SupervisorConfig, TaskDefaults};
//!
//! let runtime = SupervisorConfig::default()
//!     .with_grace(Duration::from_secs(30))
//!     .with_max_concurrent(NonZeroUsize::new(4));
//! let tasks = TaskDefaults::default().with_timeout(Duration::from_secs(5));
//!
//! let supervisor = SupervisorBuilder::new(runtime)
//!     .with_task_defaults(tasks)
//!     .build();
//! ```

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use tokio::{sync, sync::mpsc};

use super::{
    alive::AliveTracker,
    registry::Registry,
    runtime::{CoreSettings, SupervisorCore},
    supervisor::Supervisor,
};
use crate::{
    core::{ConfigError, SupervisorConfig, TaskDefaults},
    events::Bus,
    subscribers::{Subscribe, SubscriberSet},
};

/// Builder for a [`Supervisor`].
///
/// Runtime limits and task defaults are separate:
///
/// ```text
/// SupervisorConfig ── runtime limits ──┐
/// TaskDefaults ────── task defaults ───┼──► SupervisorBuilder ──► Supervisor
/// subscribers ─────── observability ───┘
/// ```
///
/// The built supervisor stays stopped until `run` or `serve` starts it.
#[must_use]
pub struct SupervisorBuilder {
    runtime: SupervisorConfig,
    task_defaults: TaskDefaults,
    subscribers: Vec<Arc<dyn Subscribe>>,

    #[cfg(feature = "controller")]
    controller_config: Option<crate::controller::ControllerConfig>,
}

impl SupervisorBuilder {
    /// Creates a builder with runtime settings and [`TaskDefaults::default`].
    pub fn new(runtime: SupervisorConfig) -> Self {
        Self {
            runtime,
            task_defaults: TaskDefaults::default(),
            subscribers: Vec::new(),

            #[cfg(feature = "controller")]
            controller_config: None,
        }
    }

    /// Replaces all runtime settings.
    pub fn with_runtime_config(mut self, runtime: SupervisorConfig) -> Self {
        self.runtime = runtime;
        self
    }

    /// Replaces all task defaults.
    pub fn with_task_defaults(mut self, task_defaults: TaskDefaults) -> Self {
        self.task_defaults = task_defaults;
        self
    }

    /// Sets the cooperative task-stop window before abort.
    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.runtime = self.runtime.with_grace(grace);
        self
    }

    /// Sets the shared deadline for draining subscriber queues.
    ///
    /// The deadline can drop queued events, but it cannot interrupt a subscriber callback already running.
    pub fn with_subscriber_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.runtime = self.runtime.with_subscriber_shutdown_timeout(timeout);
        self
    }

    /// Sets or clears the limit for task attempts running at the same time.
    ///
    /// Pass a [`NonZeroUsize`] for a limit or `None` for no limit.
    pub fn with_max_concurrent(mut self, max_concurrent: impl Into<Option<NonZeroUsize>>) -> Self {
        self.runtime = self.runtime.with_max_concurrent(max_concurrent.into());
        self
    }

    /// Sets the concurrency limit from a raw integer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `max_concurrent` is zero.
    pub fn try_with_max_concurrent(mut self, max_concurrent: usize) -> Result<Self, ConfigError> {
        self.runtime = self.runtime.try_with_max_concurrent(max_concurrent)?;
        Ok(self)
    }

    /// Sets how many recent events the broadcast bus keeps.
    pub fn with_bus_capacity(mut self, bus_capacity: NonZeroUsize) -> Self {
        self.runtime = self.runtime.with_bus_capacity(bus_capacity);
        self
    }

    /// Sets the event-bus capacity from a raw integer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `bus_capacity` is zero.
    pub fn try_with_bus_capacity(mut self, bus_capacity: usize) -> Result<Self, ConfigError> {
        self.runtime = self.runtime.try_with_bus_capacity(bus_capacity)?;
        Ok(self)
    }

    /// Sets the registry management-queue capacity.
    pub fn with_registry_queue_capacity(mut self, registry_queue_capacity: NonZeroUsize) -> Self {
        self.runtime = self
            .runtime
            .with_registry_queue_capacity(registry_queue_capacity);
        self
    }

    /// Sets the registry queue capacity from a raw integer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `registry_queue_capacity` is zero.
    pub fn try_with_registry_queue_capacity(
        mut self,
        registry_queue_capacity: usize,
    ) -> Result<Self, ConfigError> {
        self.runtime = self
            .runtime
            .try_with_registry_queue_capacity(registry_queue_capacity)?;
        Ok(self)
    }

    /// Replaces the subscribers that receive best-effort lifecycle events.
    pub fn with_subscribers(mut self, subscribers: Vec<Arc<dyn Subscribe>>) -> Self {
        self.subscribers = subscribers;
        self
    }

    /// Configures slot admission for `SupervisorHandle::submit*` methods.
    ///
    /// Direct `add*` methods bypass the controller and register with the runtime.
    #[cfg(feature = "controller")]
    pub fn with_controller(mut self, config: crate::controller::ControllerConfig) -> Self {
        self.controller_config = Some(config);
        self
    }

    /// Builds a stopped supervisor.
    ///
    /// It is safe to call outside Tokio.
    /// The method allocates channels and stores configuration, but does not spawn tasks.
    #[must_use]
    pub fn build(self) -> Arc<Supervisor> {
        let bus = Bus::new(self.runtime.bus_capacity().get());
        let subs = Arc::new(SubscriberSet::new_with_shutdown_timeout(
            self.subscribers,
            bus.clone(),
            self.runtime.subscriber_shutdown_timeout(),
        ));
        let runtime_token = tokio_util::sync::CancellationToken::new();

        let semaphore = self
            .runtime
            .max_concurrent()
            .map(|limit| Arc::new(sync::Semaphore::new(limit.get())));

        let (cmd_tx, cmd_rx) = mpsc::channel(self.runtime.registry_queue_capacity().get());
        let registry = Registry::new(
            bus.clone(),
            runtime_token.clone(),
            semaphore,
            self.runtime.grace(),
            self.task_defaults.clone(),
            cmd_rx,
        );
        let alive = Arc::new(AliveTracker::new());

        let core = SupervisorCore::new_internal(
            CoreSettings::new(self.runtime, self.task_defaults),
            bus.clone(),
            subs,
            alive,
            registry,
            runtime_token,
            cmd_tx,
        );

        #[cfg(feature = "controller")]
        let controller = self
            .controller_config
            .map(|config| crate::controller::Controller::new(config, &core, bus.clone()));

        #[cfg(feature = "controller")]
        if let Some(controller) = &controller {
            core.attach_controller(controller);
        }

        Supervisor::from_parts(
            core,
            #[cfg(feature = "controller")]
            controller,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackoffPolicy, RestartPolicy};
    use std::num::NonZeroU32;

    #[test]
    fn builder_keeps_runtime_and_task_defaults_separate() {
        let runtime = SupervisorConfig::default()
            .with_grace(Duration::from_secs(30))
            .with_subscriber_shutdown_timeout(Duration::from_secs(2))
            .with_max_concurrent(NonZeroUsize::new(4))
            .with_bus_capacity(NonZeroUsize::new(2048).unwrap())
            .with_registry_queue_capacity(NonZeroUsize::new(256).unwrap());
        let task_defaults = TaskDefaults::default()
            .with_restart(RestartPolicy::Never)
            .with_backoff(BackoffPolicy::constant(Duration::from_millis(50)))
            .with_timeout(Duration::from_secs(5))
            .with_max_retries(NonZeroU32::new(10));

        let builder =
            SupervisorBuilder::new(runtime.clone()).with_task_defaults(task_defaults.clone());

        assert_eq!(builder.runtime.grace(), runtime.grace());
        assert_eq!(
            builder.runtime.subscriber_shutdown_timeout(),
            runtime.subscriber_shutdown_timeout()
        );
        assert_eq!(builder.runtime.max_concurrent(), runtime.max_concurrent());
        assert_eq!(builder.runtime.bus_capacity(), runtime.bus_capacity());
        assert_eq!(
            builder.runtime.registry_queue_capacity(),
            runtime.registry_queue_capacity()
        );
        assert!(matches!(
            builder.task_defaults.restart(),
            RestartPolicy::Never
        ));
        assert_eq!(
            builder.task_defaults.backoff().first(),
            Duration::from_millis(50)
        );
        assert_eq!(
            builder.task_defaults.timeout(),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            builder.task_defaults.max_retries().map(NonZeroU32::get),
            Some(10)
        );
    }

    #[test]
    fn raw_zero_values_return_errors_instead_of_panicking() {
        type RawSetter = fn(SupervisorBuilder, usize) -> Result<SupervisorBuilder, ConfigError>;
        let cases: [(&str, RawSetter); 3] = [
            ("max_concurrent", SupervisorBuilder::try_with_max_concurrent),
            ("bus_capacity", SupervisorBuilder::try_with_bus_capacity),
            (
                "registry_queue_capacity",
                SupervisorBuilder::try_with_registry_queue_capacity,
            ),
        ];

        for (field, set) in cases {
            assert!(matches!(
                set(SupervisorBuilder::new(SupervisorConfig::default()), 0),
                Err(ConfigError::Zero { field: actual }) if actual == field
            ));
        }
    }

    #[test]
    fn max_concurrent_accepts_a_limit_or_an_option() {
        let limit = NonZeroUsize::new(4).unwrap();
        let direct = SupervisorBuilder::new(SupervisorConfig::default()).with_max_concurrent(limit);
        let optional =
            SupervisorBuilder::new(SupervisorConfig::default()).with_max_concurrent(Some(limit));
        let cleared = SupervisorBuilder::new(SupervisorConfig::default()).with_max_concurrent(None);

        assert_eq!(direct.runtime.max_concurrent(), Some(limit));
        assert_eq!(optional.runtime.max_concurrent(), Some(limit));
        assert_eq!(cleared.runtime.max_concurrent(), None);
    }
}
