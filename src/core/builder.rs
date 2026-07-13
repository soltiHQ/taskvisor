//! Builder for a stopped [`Supervisor`](crate::Supervisor).
//!
//! Construction is runtime-independent: [`build`](SupervisorBuilder::build)
//! does not spawn Tokio tasks. Workers start later in `run()` or `serve()`.
//!
//! ```rust
//! use std::num::NonZeroUsize;
//! use std::time::Duration;
//! use taskvisor::{SupervisorBuilder, SupervisorConfig, TaskDefaults};
//!
//! let runtime = SupervisorConfig::default()
//!     .with_grace(Duration::from_secs(30))
//!     .with_max_concurrent(NonZeroUsize::new(4));
//! let tasks = TaskDefaults::default().with_timeout(Some(Duration::from_secs(5)));
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

/// Builder for constructing a [`Supervisor`](crate::Supervisor).
///
/// Runtime settings and task defaults are separate inputs. The produced
/// supervisor is inert until [`Supervisor::run`](crate::Supervisor::run) or
/// [`Supervisor::serve`](crate::Supervisor::serve) starts it.
pub struct SupervisorBuilder {
    runtime: SupervisorConfig,
    task_defaults: TaskDefaults,
    subscribers: Vec<Arc<dyn Subscribe>>,

    #[cfg(feature = "controller")]
    controller_config: Option<crate::controller::ControllerConfig>,
}

impl SupervisorBuilder {
    /// Creates a builder with the provided runtime settings and
    /// [`TaskDefaults::default`] for task execution.
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
    #[must_use]
    pub fn with_runtime_config(mut self, runtime: SupervisorConfig) -> Self {
        self.runtime = runtime;
        self
    }

    /// Replaces all task defaults.
    #[must_use]
    pub fn with_task_defaults(mut self, task_defaults: TaskDefaults) -> Self {
        self.task_defaults = task_defaults;
        self
    }

    /// Sets the graceful task-shutdown window.
    #[must_use]
    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.runtime = self.runtime.with_grace(grace);
        self
    }

    /// Sets the shared subscriber-drain timeout.
    #[must_use]
    pub fn with_subscriber_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.runtime = self.runtime.with_subscriber_shutdown_timeout(timeout);
        self
    }

    /// Sets or clears the global task-attempt concurrency limit.
    #[must_use]
    pub fn with_max_concurrent(mut self, max_concurrent: Option<NonZeroUsize>) -> Self {
        self.runtime = self.runtime.with_max_concurrent(max_concurrent);
        self
    }

    /// Convenience setter that validates a raw concurrency limit.
    ///
    /// # Errors
    /// Returns [`ConfigError::Zero`] when `max_concurrent` is zero.
    pub fn try_with_max_concurrent(mut self, max_concurrent: usize) -> Result<Self, ConfigError> {
        self.runtime = self.runtime.try_with_max_concurrent(max_concurrent)?;
        Ok(self)
    }

    /// Sets the non-zero runtime event-bus capacity.
    #[must_use]
    pub fn with_bus_capacity(mut self, bus_capacity: NonZeroUsize) -> Self {
        self.runtime = self.runtime.with_bus_capacity(bus_capacity);
        self
    }

    /// Convenience setter that validates a raw event-bus capacity.
    ///
    /// # Errors
    /// Returns [`ConfigError::Zero`] when `bus_capacity` is zero.
    pub fn try_with_bus_capacity(mut self, bus_capacity: usize) -> Result<Self, ConfigError> {
        self.runtime = self.runtime.try_with_bus_capacity(bus_capacity)?;
        Ok(self)
    }

    /// Sets the non-zero registry management-queue capacity.
    #[must_use]
    pub fn with_registry_queue_capacity(mut self, registry_queue_capacity: NonZeroUsize) -> Self {
        self.runtime = self
            .runtime
            .with_registry_queue_capacity(registry_queue_capacity);
        self
    }

    /// Convenience setter that validates a raw registry queue capacity.
    ///
    /// # Errors
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

    /// Replaces event subscribers used for runtime observability.
    #[must_use]
    pub fn with_subscribers(mut self, subscribers: Vec<Arc<dyn Subscribe>>) -> Self {
        self.subscribers = subscribers;
        self
    }

    /// Enables slot-based controller admission.
    #[cfg(feature = "controller")]
    #[must_use]
    pub fn with_controller(mut self, config: crate::controller::ControllerConfig) -> Self {
        self.controller_config = Some(config);
        self
    }

    /// Builds an inert supervisor.
    ///
    /// This method is safe outside a Tokio runtime. It allocates channels and
    /// stores subscribers, but does not spawn listeners, subscriber workers, or
    /// controller tasks.
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
            .with_timeout(Some(Duration::from_secs(5)))
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
        assert!(matches!(
            SupervisorBuilder::new(SupervisorConfig::default()).try_with_max_concurrent(0),
            Err(ConfigError::Zero {
                field: "max_concurrent"
            })
        ));
        assert!(matches!(
            SupervisorBuilder::new(SupervisorConfig::default()).try_with_bus_capacity(0),
            Err(ConfigError::Zero {
                field: "bus_capacity"
            })
        ));
        assert!(matches!(
            SupervisorBuilder::new(SupervisorConfig::default()).try_with_registry_queue_capacity(0),
            Err(ConfigError::Zero {
                field: "registry_queue_capacity"
            })
        ));
    }
}
