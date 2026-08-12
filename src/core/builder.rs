//! Build a stopped [`Supervisor`].
//!
//! [`SupervisorBuilder::build`] only creates the runtime state and channels.
//! It does not spawn Tokio tasks.
//! A non-empty subscriber set also reserves shared ownership and may lazily
//! initialize the process-wide destructor-isolation threads.
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
    deferred_drop,
    registry::Registry,
    runtime::{CoreSettings, SupervisorCore},
    supervisor::Supervisor,
};
use crate::{
    BuildError,
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

    /// Sets the cooperative task-stop window before logical force-abort.
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
    /// [`try_build`](Self::try_build) rejects values above the bounded async implementation limit.
    pub fn with_max_concurrent(mut self, max_concurrent: impl Into<Option<NonZeroUsize>>) -> Self {
        self.runtime = self.runtime.with_max_concurrent(max_concurrent.into());
        self
    }

    /// Sets the concurrency limit from a raw integer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `max_concurrent` is zero, or
    /// [`ConfigError::TooLarge`] above the bounded async implementation limit.
    pub fn try_with_max_concurrent(mut self, max_concurrent: usize) -> Result<Self, ConfigError> {
        self.runtime = self.runtime.try_with_max_concurrent(max_concurrent)?;
        Ok(self)
    }

    /// Sets or clears the registry membership limit.
    ///
    /// Registered and removing tasks count until terminal cleanup finishes. Force-aborted attempts
    /// still being physically reaped also consume this budget.
    /// Passing `None` disables only this per-supervisor registry limit; the
    /// process-wide `owned_user_lifetimes` budget of 1024 tasks and subscribers remains active.
    pub fn with_max_registered_tasks(
        mut self,
        max_registered_tasks: impl Into<Option<NonZeroUsize>>,
    ) -> Self {
        self.runtime = self
            .runtime
            .with_max_registered_tasks(max_registered_tasks.into());
        self
    }

    /// Sets the registry membership limit from a raw integer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `max_registered_tasks` is zero.
    pub fn try_with_max_registered_tasks(
        mut self,
        max_registered_tasks: usize,
    ) -> Result<Self, ConfigError> {
        self.runtime = self
            .runtime
            .try_with_max_registered_tasks(max_registered_tasks)?;
        Ok(self)
    }

    /// Sets the event-bus capacity from a raw integer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `bus_capacity` is zero, or
    /// [`ConfigError::TooLarge`] above the bounded async implementation limit.
    pub fn try_with_bus_capacity(mut self, bus_capacity: usize) -> Result<Self, ConfigError> {
        self.runtime = self.runtime.try_with_bus_capacity(bus_capacity)?;
        Ok(self)
    }

    /// Sets the registry management-queue capacity.
    ///
    /// [`try_build`](Self::try_build) rejects values above the bounded async implementation limit.
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
    /// Returns [`ConfigError::Zero`] when `registry_queue_capacity` is zero, or
    /// [`ConfigError::TooLarge`] above the bounded async implementation limit.
    pub fn try_with_registry_queue_capacity(
        mut self,
        registry_queue_capacity: usize,
    ) -> Result<Self, ConfigError> {
        self.runtime = self
            .runtime
            .try_with_registry_queue_capacity(registry_queue_capacity)?;
        Ok(self)
    }

    /// Sets how many newest events the event ingress retains.
    ///
    /// [`try_build`](Self::try_build) rejects values above the bounded async implementation limit.
    pub fn with_bus_capacity(mut self, bus_capacity: NonZeroUsize) -> Self {
        self.runtime = self.runtime.with_bus_capacity(bus_capacity);
        self
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
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    pub fn with_controller(mut self, config: crate::controller::ControllerConfig) -> Self {
        self.controller_config = Some(config);
        self
    }

    /// Builds a stopped supervisor.
    ///
    /// It is safe to call outside Tokio.
    /// The method allocates channels and stores configuration, but does not spawn Tokio tasks.
    /// Configured subscribers reserve process-wide ownership slots and may initialize the
    /// two shared destructor-isolation threads.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use std::time::Duration;
    /// use taskvisor::{SupervisorBuilder, SupervisorConfig};
    ///
    /// let supervisor = SupervisorBuilder::new(SupervisorConfig::default())
    ///     .with_grace(Duration::from_secs(15))
    ///     .build();
    ///
    /// assert_eq!(
    ///     supervisor.runtime_config().grace(),
    ///     Duration::from_secs(15)
    /// );
    /// ```
    ///
    /// # Panics
    ///
    /// Panics with a typed build error message when Taskvisor cannot reserve one
    /// process-wide library-owned user-lifetime slot per configured subscriber
    /// or when a bounded async capacity is structurally too large.
    /// A panic from [`Subscribe::name`] or [`Subscribe::queue_capacity`] also
    /// propagates after subscriber ownership has entered destructor isolation.
    /// Use [`try_build`](Self::try_build) to handle resource exhaustion.
    #[must_use]
    pub fn build(self) -> Arc<Supervisor> {
        self.try_build().unwrap_or_else(|error| {
            panic!(
                "SupervisorBuilder::build rejected its configuration: {error}; use SupervisorBuilder::try_build for a typed error"
            )
        })
    }

    /// Tries to build a stopped supervisor.
    ///
    /// Subscriber ownership is reserved as one atomic batch before Taskvisor
    /// calls [`Subscribe::name`] or [`Subscribe::queue_capacity`]. A rejected
    /// batch therefore invokes neither subscriber metadata callback and consumes no
    /// ownership slots.
    ///
    /// This method is safe to call outside Tokio and does not spawn Tokio tasks.
    /// Configured subscribers may initialize the two shared destructor-isolation threads.
    ///
    /// # Errors
    ///
    /// - [`BuildError::ResourceLimitReached`] when the process-wide
    ///   library-owned user-lifetime budget cannot admit every subscriber.
    /// - [`BuildError::CapacityTooLarge`] when a runtime, controller, or
    ///   subscriber capacity exceeds the bounded async implementation limit.
    ///
    /// # Panics
    ///
    /// A panic from [`Subscribe::name`] or [`Subscribe::queue_capacity`]
    /// continues to the caller after every configured subscriber has been
    /// transferred into charged destructor isolation.
    pub fn try_build(self) -> Result<Arc<Supervisor>, BuildError> {
        self.validate_configuration()?;
        let reservations = deferred_drop::try_reserve_many(self.subscribers.len())
            .map_err(Self::ownership_build_error)?;
        self.build_with_reservations(reservations)
    }

    #[cfg(test)]
    fn try_build_with_reservation_source(
        self,
        source: &deferred_drop::TestReservationSource,
    ) -> Result<Arc<Supervisor>, BuildError> {
        self.validate_configuration()?;
        let reservations = source
            .try_reserve_many(self.subscribers.len())
            .map_err(Self::ownership_build_error)?;
        self.build_with_reservations(reservations)
    }

    fn validate_configuration(&self) -> Result<(), BuildError> {
        self.runtime
            .validate()
            .map_err(Self::configuration_build_error)?;
        #[cfg(feature = "controller")]
        if let Some(config) = &self.controller_config {
            config.validate().map_err(Self::configuration_build_error)?;
        }
        Ok(())
    }

    fn configuration_build_error(error: ConfigError) -> BuildError {
        match error {
            ConfigError::TooLarge { field, value, max } => {
                BuildError::CapacityTooLarge { field, value, max }
            }
            ConfigError::Zero { .. } => {
                unreachable!("stored capacities use NonZeroUsize")
            }
        }
    }

    fn ownership_build_error(error: deferred_drop::DropCapacityError) -> BuildError {
        BuildError::ResourceLimitReached {
            resource: deferred_drop::OWNERSHIP_RESOURCE,
            limit: error.limit(),
        }
    }

    fn build_with_reservations(
        self,
        reservations: Vec<deferred_drop::DropReservation>,
    ) -> Result<Arc<Supervisor>, BuildError> {
        let bus = Bus::new(self.runtime.bus_capacity().get());
        let subs = Arc::new(SubscriberSet::from_reserved(
            self.subscribers,
            reservations,
            bus.clone(),
            self.runtime.subscriber_shutdown_timeout(),
        )?);
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
            self.runtime.max_registered_tasks(),
            cmd_rx,
        );
        let core = SupervisorCore::new_internal(
            CoreSettings::new(self.runtime, self.task_defaults),
            bus.clone(),
            subs,
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

        Ok(Supervisor::from_parts(
            core,
            #[cfg(feature = "controller")]
            controller,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackoffPolicy, RestartPolicy};
    use std::num::NonZeroU32;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MetadataProbe {
        name_calls: Arc<AtomicUsize>,
        capacity_calls: Arc<AtomicUsize>,
    }

    impl Subscribe for MetadataProbe {
        fn on_event(&self, _event: &crate::Event) {}

        fn name(&self) -> &str {
            self.name_calls.fetch_add(1, Ordering::AcqRel);
            "metadata-probe"
        }

        fn queue_capacity(&self) -> NonZeroUsize {
            self.capacity_calls.fetch_add(1, Ordering::AcqRel);
            NonZeroUsize::new(8).expect("test capacity is non-zero")
        }
    }

    struct OversizedQueueSubscriber;

    impl Subscribe for OversizedQueueSubscriber {
        fn on_event(&self, _event: &crate::Event) {}

        fn queue_capacity(&self) -> NonZeroUsize {
            NonZeroUsize::new(crate::core::MAX_ASYNC_CAPACITY + 1)
                .expect("the excessive test value is non-zero")
        }
    }

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

    #[test]
    fn try_build_reserves_the_complete_subscriber_batch_before_subscriber_metadata() {
        let source = deferred_drop::TestReservationSource::new(1);
        let name_calls = Arc::new(AtomicUsize::new(0));
        let capacity_calls = Arc::new(AtomicUsize::new(0));
        let subscribers: Vec<Arc<dyn Subscribe>> = (0..2)
            .map(|_| {
                Arc::new(MetadataProbe {
                    name_calls: Arc::clone(&name_calls),
                    capacity_calls: Arc::clone(&capacity_calls),
                }) as Arc<dyn Subscribe>
            })
            .collect();

        let result = SupervisorBuilder::new(SupervisorConfig::default())
            .with_subscribers(subscribers)
            .try_build_with_reservation_source(&source);

        assert!(matches!(
            result,
            Err(BuildError::ResourceLimitReached {
                resource: deferred_drop::OWNERSHIP_RESOURCE,
                limit: deferred_drop::OWNERSHIP_CAPACITY,
                ..
            })
        ));
        assert_eq!(name_calls.load(Ordering::Acquire), 0);
        assert_eq!(capacity_calls.load(Ordering::Acquire), 0);
        let untouched = source
            .try_reserve()
            .expect("an atomic rejection cannot retain partial capacity");
        drop(untouched);
    }

    #[test]
    fn try_build_rejects_structurally_invalid_runtime_capacities() {
        let excessive = NonZeroUsize::new(crate::core::MAX_ASYNC_CAPACITY + 1)
            .expect("the excessive test value is non-zero");
        let cases = [
            (
                SupervisorConfig::default().with_max_concurrent(Some(excessive)),
                "max_concurrent",
            ),
            (
                SupervisorConfig::default().with_bus_capacity(excessive),
                "bus_capacity",
            ),
            (
                SupervisorConfig::default().with_registry_queue_capacity(excessive),
                "registry_queue_capacity",
            ),
        ];

        for (runtime, field) in cases {
            assert!(matches!(
                SupervisorBuilder::new(runtime).try_build(),
                Err(BuildError::CapacityTooLarge {
                    field: actual,
                    value,
                    max: crate::core::MAX_ASYNC_CAPACITY,
                    ..
                }) if actual == field && value == excessive.get()
            ));
        }
    }

    #[cfg(feature = "controller")]
    #[test]
    fn try_build_rejects_structurally_invalid_controller_capacity() {
        let excessive = NonZeroUsize::new(crate::core::MAX_ASYNC_CAPACITY + 1)
            .expect("the excessive test value is non-zero");
        let config = crate::ControllerConfig::default().with_queue_capacity(excessive);

        assert!(matches!(
            SupervisorBuilder::new(SupervisorConfig::default())
                .with_controller(config)
                .try_build(),
            Err(BuildError::CapacityTooLarge {
                field: "controller_queue_capacity",
                value,
                max: crate::core::MAX_ASYNC_CAPACITY,
                ..
            }) if value == excessive.get()
        ));
    }

    #[test]
    fn try_build_rejects_oversized_subscriber_queue_before_runtime_start() {
        let source = deferred_drop::TestReservationSource::new(1);
        let result = SupervisorBuilder::new(SupervisorConfig::default())
            .with_subscribers(vec![Arc::new(OversizedQueueSubscriber)])
            .try_build_with_reservation_source(&source);

        assert!(matches!(
            result,
            Err(BuildError::CapacityTooLarge {
                field: "subscriber_queue_capacity",
                value,
                max: crate::core::MAX_ASYNC_CAPACITY,
                ..
            }) if value == crate::core::MAX_ASYNC_CAPACITY + 1
        ));
    }
}
