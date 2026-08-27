//! Assembles the configuration and resources for a stopped [`Supervisor`].
//!
//! [`SupervisorBuilder`] combines runtime limits, task defaults, subscribers, and optional controller settings.
//! Building validates bounded capacities, creates the runtime state and channels, and reserves subscriber ownership.
//! It does not spawn Tokio tasks.
//!
//! ```text
//! application
//!      │ SupervisorConfig + TaskDefaults + subscribers
//!      ▼
//! SupervisorBuilder
//!      ▼
//! stopped Supervisor
//!      │ run* or serve
//!      ▼
//! running runtime
//! ```
//!
//! With subscribers, construction starts background cleanup workers before reading subscriber metadata.
//! These workers destroy retained user values outside Tokio runtime paths.
//! Without subscribers, they stay dormant until the first task or controller ownership admission.

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
    events::{Bus, Event},
    subscribers::{Subscribe, SubscriberSet},
};

/// Collects all immutable settings used by one [`Supervisor`].
///
/// Use [`Supervisor::new`] when runtime configuration and subscribers are enough. Use this builder to change
/// inherited task settings, enable controller admission, or handle construction failure with [`try_build`](Self::try_build).
///
/// Setter calls consume and return the builder. Calling [`with_runtime_config`](Self::with_runtime_config) replaces changes
/// made by earlier runtime-setting shortcuts. Call [`build`](Self::build) for a convenience panic on failure
/// or [`try_build`](Self::try_build) for a typed [`BuildError`].
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
    ///
    /// No subscribers or controller are configured initially.
    pub fn new(runtime: SupervisorConfig) -> Self {
        Self {
            runtime,
            task_defaults: TaskDefaults::default(),
            subscribers: Vec::new(),

            #[cfg(feature = "controller")]
            controller_config: None,
        }
    }

    /// Replaces all runtime settings stored by this builder.
    ///
    /// This also replaces values set by earlier runtime-setting shortcuts such as [`with_grace`](Self::with_grace).
    pub fn with_runtime_config(mut self, runtime: SupervisorConfig) -> Self {
        self.runtime = runtime;
        self
    }

    /// Replaces defaults used by inherited [`TaskSpec`](crate::TaskSpec) settings.
    pub fn with_task_defaults(mut self, task_defaults: TaskDefaults) -> Self {
        self.task_defaults = task_defaults;
        self
    }

    /// Sets the cooperative task-stop window before logical force-abort.
    ///
    /// See [`SupervisorConfig::with_grace`] for normalization and zero behavior.
    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.runtime = self.runtime.with_grace(grace);
        self
    }

    /// Sets the shared deadline for draining subscriber queues during shutdown.
    ///
    /// See [`SupervisorConfig::with_subscriber_shutdown_timeout`] for the callback boundary.
    pub fn with_subscriber_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.runtime = self.runtime.with_subscriber_shutdown_timeout(timeout);
        self
    }

    /// Sets or clears the limit for task attempts running at the same time.
    ///
    /// [`try_build`](Self::try_build) validates the stored concurrency limit.
    pub fn with_max_concurrent(mut self, max_concurrent: impl Into<Option<NonZeroUsize>>) -> Self {
        self.runtime = self.runtime.with_max_concurrent(max_concurrent.into());
        self
    }

    /// Sets the task-attempt concurrency limit from a raw integer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] for zero or [`ConfigError::TooLarge`] above Tokio's structural limit.
    pub fn try_with_max_concurrent(mut self, max_concurrent: usize) -> Result<Self, ConfigError> {
        self.runtime = self.runtime.try_with_max_concurrent(max_concurrent)?;
        Ok(self)
    }

    /// Sets or clears the registry membership limit.
    ///
    /// See [`SupervisorConfig::with_max_registered_tasks`] for which lifecycle phases consume the limit.
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

    /// Sets or clears the limit for user lifetimes owned by this supervisor.
    ///
    /// See [`SupervisorConfig::with_ownership_capacity`] for the shared task and
    /// subscriber contract.
    pub fn with_ownership_capacity(
        mut self,
        ownership_capacity: impl Into<Option<NonZeroUsize>>,
    ) -> Self {
        self.runtime = self
            .runtime
            .with_ownership_capacity(ownership_capacity.into());
        self
    }

    /// Sets the user-lifetime ownership limit from a raw integer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `ownership_capacity` is zero.
    pub fn try_with_ownership_capacity(
        mut self,
        ownership_capacity: usize,
    ) -> Result<Self, ConfigError> {
        self.runtime = self
            .runtime
            .try_with_ownership_capacity(ownership_capacity)?;
        Ok(self)
    }

    /// Sets the best-effort event-bus capacity from a raw integer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] for zero or [`ConfigError::TooLarge`] above Tokio's structural limit.
    pub fn try_with_bus_capacity(mut self, bus_capacity: usize) -> Result<Self, ConfigError> {
        self.runtime = self.runtime.try_with_bus_capacity(bus_capacity)?;
        Ok(self)
    }

    /// Sets the bounded registry management-queue capacity.
    ///
    /// [`try_build`](Self::try_build) validates the stored queue capacity.
    pub fn with_registry_queue_capacity(mut self, registry_queue_capacity: NonZeroUsize) -> Self {
        self.runtime = self
            .runtime
            .with_registry_queue_capacity(registry_queue_capacity);
        self
    }

    /// Sets the registry management-queue capacity from a raw integer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] for zero or [`ConfigError::TooLarge`] above Tokio's structural limit.
    pub fn try_with_registry_queue_capacity(
        mut self,
        registry_queue_capacity: usize,
    ) -> Result<Self, ConfigError> {
        self.runtime = self
            .runtime
            .try_with_registry_queue_capacity(registry_queue_capacity)?;
        Ok(self)
    }

    /// Sets how many newest events the best-effort event bus retains.
    ///
    /// [`try_build`](Self::try_build) validates the stored event capacity.
    pub fn with_bus_capacity(mut self, bus_capacity: NonZeroUsize) -> Self {
        self.runtime = self.runtime.with_bus_capacity(bus_capacity);
        self
    }

    /// Replaces all subscribers that receive best-effort lifecycle events.
    ///
    /// An empty vector disables the event bus and subscriber workers.
    pub fn with_subscribers(mut self, subscribers: Vec<Arc<dyn Subscribe>>) -> Self {
        self.subscribers = subscribers;
        self
    }

    /// Enables slot admission for `SupervisorHandle::submit*` methods.
    ///
    /// Without this setting, `submit*` methods return [`ControllerError::NotConfigured`](crate::ControllerError::NotConfigured).
    /// Direct `add*` methods always bypass the controller and register with the runtime.
    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    pub fn with_controller(mut self, config: crate::controller::ControllerConfig) -> Self {
        self.controller_config = Some(config);
        self
    }

    /// Builds a stopped supervisor and panics if construction fails.
    ///
    /// This method is safe to call outside Tokio and does not spawn Tokio tasks.
    /// Use [`try_build`](Self::try_build) when the application must report or recover from resource and capacity failures.
    ///
    /// # Examples
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
    /// Panics when [`try_build`](Self::try_build) would return an error.
    /// A panic from [`Subscribe::execution`], [`Subscribe::name`], or [`Subscribe::queue_capacity`]
    /// also reaches the caller.
    #[must_use]
    pub fn build(self) -> Arc<Supervisor> {
        self.try_build().unwrap_or_else(|error| {
            panic!(
                "SupervisorBuilder::build rejected its configuration: {error}; use SupervisorBuilder::try_build for a typed error"
            )
        })
    }

    /// Builds a stopped supervisor and returns typed construction failures.
    ///
    /// This method is safe to call outside Tokio and does not spawn Tokio tasks.
    /// It reserves all subscriber ownership slots as one batch before calling [`Subscribe::execution`],
    /// [`Subscribe::name`], or [`Subscribe::queue_capacity`].
    /// A rejected batch calls none of these methods and keeps no ownership slots.
    ///
    /// # Errors
    ///
    /// - [`BuildError::ResourceLimitReached`] when the supervisor cannot own every configured subscriber.
    /// - [`BuildError::ThreadStartFailed`] when background cleanup workers cannot start.
    /// - [`BuildError::CapacityTooLarge`] when a runtime, controller, or subscriber capacity exceeds Tokio's structural limit.
    ///
    /// # Panics
    ///
    /// A panic from [`Subscribe::execution`], [`Subscribe::name`], or [`Subscribe::queue_capacity`] reaches the caller
    /// after Taskvisor reserves subscriber ownership for deferred cleanup.
    pub fn try_build(self) -> Result<Arc<Supervisor>, BuildError> {
        self.validate_configuration()?;
        let drop_domain = deferred_drop::DropDomain::unstarted(self.runtime.ownership_capacity());
        let reservations = drop_domain
            .try_reserve_many(self.subscribers.len())
            .map_err(Self::ownership_admission_build_error)?;
        self.build_with_reservations(drop_domain, reservations)
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
        self.build_with_reservations(source.domain(), reservations)
    }

    #[cfg(test)]
    fn try_build_with_drop_domain(
        self,
        drop_domain: deferred_drop::DropDomain,
    ) -> Result<Arc<Supervisor>, BuildError> {
        self.validate_configuration()?;
        let reservations = drop_domain
            .try_reserve_many(self.subscribers.len())
            .map_err(Self::ownership_admission_build_error)?;
        self.build_with_reservations(drop_domain, reservations)
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
        let limit = error
            .limit()
            .expect("a fresh unlimited ownership domain cannot reject capacity");
        BuildError::ResourceLimitReached {
            resource: deferred_drop::OWNERSHIP_RESOURCE,
            limit: limit.get(),
        }
    }

    fn ownership_admission_build_error(error: deferred_drop::DropAdmissionError) -> BuildError {
        match error {
            deferred_drop::DropAdmissionError::Start(error) => Self::drop_domain_build_error(error),
            deferred_drop::DropAdmissionError::Capacity(error) => {
                Self::ownership_build_error(error)
            }
        }
    }

    fn drop_domain_build_error(error: deferred_drop::DropStartError) -> BuildError {
        BuildError::ThreadStartFailed {
            component: "destructor_isolation",
            worker: error.worker(),
            kind: error.source_kind(),
            raw_os_error: error.raw_os_error(),
        }
    }

    fn build_with_reservations(
        self,
        drop_domain: deferred_drop::DropDomain,
        reservations: Vec<deferred_drop::DropReservation>,
    ) -> Result<Arc<Supervisor>, BuildError> {
        let bus = Bus::new(self.runtime.bus_capacity().get());
        let retirement_bus = bus.clone();
        drop_domain.set_retirement_reporter(move |configured, effective, retired| {
            retirement_bus.publish_lazy(|| {
                Event::ownership_capacity_retired(configured, effective, retired)
                    .with_task("destructor_isolation")
            });
        });
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
            drop_domain,
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

    struct StartupOrderingProbe {
        drop_domain: deferred_drop::DropDomain,
        name_calls: Arc<AtomicUsize>,
        capacity_calls: Arc<AtomicUsize>,
    }

    impl StartupOrderingProbe {
        fn observe_started_domain(&self) {
            assert!(
                self.drop_domain.is_started(),
                "subscriber metadata must run only after the destructor-isolation core is published"
            );
        }
    }

    impl Subscribe for StartupOrderingProbe {
        fn on_event(&self, _event: &crate::Event) {}

        fn name(&self) -> &str {
            self.observe_started_domain();
            self.name_calls.fetch_add(1, Ordering::AcqRel);
            "startup-ordering-probe"
        }

        fn queue_capacity(&self) -> NonZeroUsize {
            self.observe_started_domain();
            self.capacity_calls.fetch_add(1, Ordering::AcqRel);
            NonZeroUsize::new(8).expect("test capacity is non-zero")
        }
    }

    #[test]
    fn builder_keeps_runtime_and_task_defaults_separate() {
        let runtime = SupervisorConfig::default()
            .with_grace(Duration::from_secs(30))
            .with_subscriber_shutdown_timeout(Duration::from_secs(2))
            .with_max_concurrent(NonZeroUsize::new(4))
            .with_ownership_capacity(NonZeroUsize::new(64))
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
        assert_eq!(
            builder.runtime.ownership_capacity(),
            runtime.ownership_capacity()
        );
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
        let cases: [(&str, RawSetter); 4] = [
            ("max_concurrent", SupervisorBuilder::try_with_max_concurrent),
            (
                "ownership_capacity",
                SupervisorBuilder::try_with_ownership_capacity,
            ),
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
    fn ownership_capacity_accepts_a_limit_or_an_option() {
        let limit = NonZeroUsize::new(4).unwrap();
        let direct =
            SupervisorBuilder::new(SupervisorConfig::default()).with_ownership_capacity(limit);
        let optional = SupervisorBuilder::new(SupervisorConfig::default())
            .with_ownership_capacity(Some(limit));
        let cleared =
            SupervisorBuilder::new(SupervisorConfig::default()).with_ownership_capacity(None);

        assert_eq!(direct.runtime.ownership_capacity(), Some(limit));
        assert_eq!(optional.runtime.ownership_capacity(), Some(limit));
        assert_eq!(cleared.runtime.ownership_capacity(), None);
    }

    #[test]
    fn subscriber_free_build_leaves_destructor_isolation_dormant() {
        let supervisor = SupervisorBuilder::new(SupervisorConfig::default())
            .try_build()
            .expect("a subscriber-free supervisor must build");

        assert!(!supervisor.core().drop_domain().is_started());
    }

    #[test]
    fn subscriber_build_starts_destructor_isolation_before_metadata() {
        let drop_domain =
            deferred_drop::DropDomain::unstarted(SupervisorConfig::default().ownership_capacity());
        let name_calls = Arc::new(AtomicUsize::new(0));
        let capacity_calls = Arc::new(AtomicUsize::new(0));
        let subscriber: Arc<dyn Subscribe> = Arc::new(StartupOrderingProbe {
            drop_domain: drop_domain.clone(),
            name_calls: Arc::clone(&name_calls),
            capacity_calls: Arc::clone(&capacity_calls),
        });

        let supervisor = SupervisorBuilder::new(SupervisorConfig::default())
            .with_subscribers(vec![subscriber])
            .try_build_with_drop_domain(drop_domain.clone())
            .expect("subscriber ownership must start the destructor-isolation core");

        assert!(drop_domain.is_started());
        assert!(supervisor.core().drop_domain().is_started());
        assert_eq!(name_calls.load(Ordering::Acquire), 1);
        assert_eq!(capacity_calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn subscriber_core_start_failure_is_typed_and_skips_metadata() {
        let injected = deferred_drop::TestLazyDomain::fail_first_start_at_worker(2, 1);
        let drop_domain = injected.domain();
        let name_calls = Arc::new(AtomicUsize::new(0));
        let capacity_calls = Arc::new(AtomicUsize::new(0));
        let subscriber: Arc<dyn Subscribe> = Arc::new(MetadataProbe {
            name_calls: Arc::clone(&name_calls),
            capacity_calls: Arc::clone(&capacity_calls),
        });

        let result = SupervisorBuilder::new(SupervisorConfig::default())
            .with_subscribers(vec![subscriber])
            .try_build_with_drop_domain(drop_domain.clone());

        match result {
            Err(BuildError::ThreadStartFailed {
                component,
                worker,
                kind,
                raw_os_error,
                ..
            }) => {
                assert_eq!(component, "destructor_isolation");
                assert_eq!(worker, 1);
                assert_eq!(kind, std::io::ErrorKind::Other);
                assert_eq!(raw_os_error, None);
            }
            Err(error) => panic!("expected a typed core-start failure, got {error}"),
            Ok(_) => panic!("the injected first core startup must fail"),
        }
        assert_eq!(name_calls.load(Ordering::Acquire), 0);
        assert_eq!(capacity_calls.load(Ordering::Acquire), 0);
        assert!(!drop_domain.is_started());
        assert_eq!(injected.spawn_calls(), 2);
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
                limit: 1,
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
    fn configured_ownership_limit_rejects_subscribers_before_metadata() {
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

        let result = SupervisorBuilder::new(
            SupervisorConfig::default().with_ownership_capacity(NonZeroUsize::new(1)),
        )
        .with_subscribers(subscribers)
        .try_build();

        assert!(matches!(
            result,
            Err(BuildError::ResourceLimitReached {
                resource: deferred_drop::OWNERSHIP_RESOURCE,
                limit: 1,
                ..
            })
        ));
        assert_eq!(name_calls.load(Ordering::Acquire), 0);
        assert_eq!(capacity_calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn disabled_ownership_limit_accepts_the_same_subscriber_batch() {
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

        let supervisor =
            SupervisorBuilder::new(SupervisorConfig::default().with_ownership_capacity(None))
                .with_subscribers(subscribers)
                .try_build()
                .expect("an unlimited ownership domain must accept the subscriber batch");

        assert_eq!(supervisor.runtime_config().ownership_capacity(), None);
        assert_eq!(name_calls.load(Ordering::Acquire), 2);
        assert_eq!(capacity_calls.load(Ordering::Acquire), 2);
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
