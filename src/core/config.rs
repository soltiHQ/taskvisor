//! Defines limits and shutdown settings shared by one runtime.
//!
//! [`SupervisorConfig`] is read while [`SupervisorBuilder`](crate::SupervisorBuilder) builds the runtime.
//! The resulting settings stay immutable.
//! Per-task restart, backoff, timeout, and retry defaults belong to [`TaskDefaults`](crate::TaskDefaults).
//! Queue capacity does not make events reliable.
//! Use watched outcomes and direct management replies for application decisions.
//!
//! ```text
//! application ──► SupervisorConfig ──► SupervisorBuilder
//!                                             ▼
//!                                     runtime resources
//!                                          ├── registry ──► queue and membership limit
//!                                          ├── attempts ──► concurrency limit
//!                                          ├── ownership ──► retained user-lifetime limit
//!                                          └── lifecycle ──► shutdown and events
//! ```

use std::num::NonZeroUsize;
use std::time::Duration;

use thiserror::Error;

/// Default capacity for bounded runtime resources.
const DEFAULT_CAPACITY: NonZeroUsize = NonZeroUsize::new(1024).unwrap();

/// Default deadline for draining subscriber queues during shutdown.
const DEFAULT_SUBSCRIBER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Structural upper bound shared by Tokio bounded channels and semaphores.
pub(crate) const MAX_ASYNC_CAPACITY: usize = tokio::sync::Semaphore::MAX_PERMITS;

/// Shared Taskvisor structural-capacity check for runtime and controller configuration.
pub(crate) fn validate_async_capacity(
    field: &'static str,
    value: NonZeroUsize,
) -> Result<(), ConfigError> {
    if value.get() > MAX_ASYNC_CAPACITY {
        Err(ConfigError::TooLarge {
            field,
            value: value.get(),
            max: MAX_ASYNC_CAPACITY,
        })
    } else {
        Ok(())
    }
}

fn checked_async_capacity(field: &'static str, value: usize) -> Result<NonZeroUsize, ConfigError> {
    let value = NonZeroUsize::new(value).ok_or(ConfigError::Zero { field })?;
    validate_async_capacity(field, value)?;
    Ok(value)
}

/// Upper bound applied to the effective task-stop window.
///
/// Clamping once keeps Tokio deadline arithmetic valid.
/// It also keeps getters, timeout behavior, and diagnostics consistent.
const MAX_GRACE: Duration = Duration::from_secs(60 * 60 * 24 * 365 * 30);

const fn normalize_grace(grace: Duration) -> Duration {
    if grace.as_secs() > MAX_GRACE.as_secs()
        || (grace.as_secs() == MAX_GRACE.as_secs()
            && grace.subsec_nanos() > MAX_GRACE.subsec_nanos())
    {
        MAX_GRACE
    } else {
        grace
    }
}

/// Error from a checked configuration setter.
///
/// Match with a wildcard arm because the enum and its data-carrying variants are non-exhaustive.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ConfigError {
    /// A value that must be positive was zero.
    #[error("{field} must be greater than zero")]
    #[non_exhaustive]
    Zero {
        /// Stable configuration field name.
        field: &'static str,
    },
    /// A channel or semaphore capacity exceeds Tokio's structural maximum.
    #[error("{field} must not exceed {max}; got {value}")]
    #[non_exhaustive]
    TooLarge {
        /// Stable configuration field name.
        field: &'static str,
        /// Rejected value.
        value: usize,
        /// Largest accepted value.
        max: usize,
    },
}

/// Immutable runtime-wide settings for one supervisor.
///
/// These settings control task shutdown, subscriber draining, bounded queues, registry membership, event ingress, concurrent attempts, and user lifetimes retained for cleanup.
/// Start with [`Default`] and change the limits needed by the application.
/// Pass the result to [`Supervisor::new`](crate::Supervisor::new) or [`Supervisor::builder`](crate::Supervisor::builder).
#[derive(Clone, Debug)]
#[must_use]
pub struct SupervisorConfig {
    grace: Duration,
    subscriber_shutdown_timeout: Duration,
    max_concurrent: Option<NonZeroUsize>,
    max_registered_tasks: Option<NonZeroUsize>,
    ownership_capacity: Option<NonZeroUsize>,
    bus_capacity: NonZeroUsize,
    registry_queue_capacity: NonZeroUsize,
}

impl SupervisorConfig {
    /// Built-in runtime configuration available in const contexts.
    pub const fn new() -> Self {
        Self {
            grace: Duration::from_secs(60),
            subscriber_shutdown_timeout: DEFAULT_SUBSCRIBER_SHUTDOWN_TIMEOUT,
            max_concurrent: None,
            max_registered_tasks: Some(DEFAULT_CAPACITY),
            ownership_capacity: Some(DEFAULT_CAPACITY),
            bus_capacity: DEFAULT_CAPACITY,
            registry_queue_capacity: DEFAULT_CAPACITY,
        }
    }

    /// Cooperative task-stop window before logical force-abort.
    ///
    /// Explicit removal, requested shutdown, and natural shutdown use this window.
    /// After it expires, Taskvisor commits a logical force-abort.
    /// The physical task code may remain active after that point.
    /// Zero skips the wait.
    #[must_use]
    pub const fn grace(&self) -> Duration {
        self.grace
    }

    /// Shared deadline for draining subscriber queues.
    ///
    /// Zero closes the queues without waiting for pending events.
    /// The deadline can drop queued events.
    /// It cannot stop a callback already running.
    #[must_use]
    pub const fn subscriber_shutdown_timeout(&self) -> Duration {
        self.subscriber_shutdown_timeout
    }

    /// Limit for task attempts running at the same time.
    ///
    /// `None` means no limit.
    /// Permit waits and retry backoff do not count.
    /// A started attempt holds its permit until its physical attempt boundary exits.
    #[must_use]
    pub const fn max_concurrent(&self) -> Option<NonZeroUsize> {
        self.max_concurrent
    }

    /// Registry membership limit.
    ///
    /// Registered and removing tasks count until terminal cleanup removes their identity.
    /// Force-aborted work can keep consuming the limit after membership ends.
    /// During cleanup handoff, one task may temporarily consume two units.
    ///
    /// `None` disables this limit.
    /// The separately configured [`ownership_capacity`](Self::ownership_capacity) still applies.
    #[must_use]
    pub const fn max_registered_tasks(&self) -> Option<NonZeroUsize> {
        self.max_registered_tasks
    }

    /// Limit for user lifetimes owned by this supervisor.
    ///
    /// Configured subscribers and accepted tasks share this limit.
    /// Each accepted lifetime keeps one unit through queued work, physical execution, and isolated terminal destruction.
    /// Force-aborted work can remain charged after logical completion.
    /// A destructor panic permanently retires its unit from a finite limit.
    /// A finite limit below the internal cleanup-worker ceiling also lowers that ceiling.
    ///
    /// `None` removes the ownership admission limit.
    /// Destructor isolation and its worker ceiling remain active.
    /// Blocked destructors can then retain an unbounded number of user values and cleanup batches.
    #[must_use]
    pub const fn ownership_capacity(&self) -> Option<NonZeroUsize> {
        self.ownership_capacity
    }

    /// Number of newest events retained by the event ingress.
    ///
    /// The bus remains best-effort.
    /// When publishers outrun the relay, older events are replaced and loss is reported through overflow diagnostics.
    #[must_use]
    pub const fn bus_capacity(&self) -> NonZeroUsize {
        self.bus_capacity
    }

    /// Capacity of the registry management queue.
    ///
    /// Default direct-registry operations wait for this capacity.
    /// Operations configured with `fail_fast()` return [`RuntimeError::CommandQueueFull`](crate::RuntimeError::CommandQueueFull) when no slot is available immediately.
    #[must_use]
    pub const fn registry_queue_capacity(&self) -> NonZeroUsize {
        self.registry_queue_capacity
    }

    /// Cooperative task-stop window before logical force-abort.
    ///
    /// Values above 30 years are normalized to 30 years.
    /// [`grace`](Self::grace) always returns the effective value used by timeout logic and diagnostics.
    pub const fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = normalize_grace(grace);
        self
    }

    /// Shared deadline for draining subscriber queues.
    ///
    /// Zero closes the queues without waiting for pending events.
    /// The deadline does not interrupt a subscriber callback already running.
    pub const fn with_subscriber_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.subscriber_shutdown_timeout = timeout;
        self
    }

    /// Optional limit for task attempts running at the same time.
    ///
    /// [`SupervisorBuilder::try_build`](crate::SupervisorBuilder::try_build) rejects values above Tokio's structural limit.
    /// Use [`try_with_max_concurrent`](Self::try_with_max_concurrent) for a raw integer.
    /// Pass `None` to remove the limit.
    pub const fn with_max_concurrent(mut self, max_concurrent: Option<NonZeroUsize>) -> Self {
        self.max_concurrent = max_concurrent;
        self
    }

    /// Task-attempt concurrency limit from a raw integer.
    ///
    /// Use [`with_max_concurrent`](Self::with_max_concurrent) with `None` for no limit.
    ///
    /// # Errors
    ///
    /// - [`ConfigError::Zero`] when `max_concurrent` is zero;
    /// - [`ConfigError::TooLarge`] when `max_concurrent` exceeds Tokio's structural limit.
    pub fn try_with_max_concurrent(self, max_concurrent: usize) -> Result<Self, ConfigError> {
        let value = checked_async_capacity("max_concurrent", max_concurrent)?;
        Ok(self.with_max_concurrent(Some(value)))
    }

    /// Optional registry membership limit.
    ///
    /// Registered and removing tasks count until terminal cleanup finishes.
    /// Force-aborted work can keep consuming the limit after membership ends.
    /// `None` disables only this limit.
    /// It does not change the separate [`ownership_capacity`](Self::ownership_capacity) setting.
    ///
    /// Use [`try_with_max_registered_tasks`](Self::try_with_max_registered_tasks) for a raw integer.
    pub const fn with_max_registered_tasks(
        mut self,
        max_registered_tasks: Option<NonZeroUsize>,
    ) -> Self {
        self.max_registered_tasks = max_registered_tasks;
        self
    }

    /// Registry membership limit from a raw integer.
    ///
    /// # Errors
    ///
    /// - [`ConfigError::Zero`] when `max_registered_tasks` is zero.
    pub fn try_with_max_registered_tasks(
        self,
        max_registered_tasks: usize,
    ) -> Result<Self, ConfigError> {
        let value = NonZeroUsize::new(max_registered_tasks).ok_or(ConfigError::Zero {
            field: "max_registered_tasks",
        })?;
        Ok(self.with_max_registered_tasks(Some(value)))
    }

    /// Optional limit for user lifetimes owned by this supervisor.
    ///
    /// Configured subscribers and accepted tasks share the limit.
    /// `None` disables ownership admission backpressure but keeps destructor isolation and its worker ceiling enabled.
    /// Without this limit, blocked destructors can retain an unbounded number of user values and cleanup batches.
    /// A finite limit below the internal cleanup-worker ceiling also lowers that ceiling.
    ///
    /// Use [`try_with_ownership_capacity`](Self::try_with_ownership_capacity) for a raw integer.
    pub const fn with_ownership_capacity(
        mut self,
        ownership_capacity: Option<NonZeroUsize>,
    ) -> Self {
        self.ownership_capacity = ownership_capacity;
        self
    }

    /// User-lifetime ownership limit from a raw integer.
    ///
    /// Use [`with_ownership_capacity`](Self::with_ownership_capacity) with `None` for no limit.
    ///
    /// # Errors
    ///
    /// - [`ConfigError::Zero`] when `ownership_capacity` is zero.
    pub fn try_with_ownership_capacity(
        self,
        ownership_capacity: usize,
    ) -> Result<Self, ConfigError> {
        let value = NonZeroUsize::new(ownership_capacity).ok_or(ConfigError::Zero {
            field: "ownership_capacity",
        })?;
        Ok(self.with_ownership_capacity(Some(value)))
    }

    /// Number of newest events retained by the event ingress.
    ///
    /// Increasing this value absorbs a larger event burst but does not make lifecycle events reliable.
    ///
    /// [`SupervisorBuilder::try_build`](crate::SupervisorBuilder::try_build) rejects values above Tokio's structural limit.
    pub const fn with_bus_capacity(mut self, bus_capacity: NonZeroUsize) -> Self {
        self.bus_capacity = bus_capacity;
        self
    }

    /// Event-bus capacity from a raw integer.
    ///
    /// # Errors
    ///
    /// - [`ConfigError::Zero`] when `bus_capacity` is zero;
    /// - [`ConfigError::TooLarge`] when `bus_capacity` exceeds Tokio's structural limit.
    pub fn try_with_bus_capacity(self, bus_capacity: usize) -> Result<Self, ConfigError> {
        let value = checked_async_capacity("bus_capacity", bus_capacity)?;
        Ok(self.with_bus_capacity(value))
    }

    /// Capacity of the registry management queue.
    ///
    /// This bounds management commands waiting for the registry.
    /// It does not change the task membership or attempt-concurrency limits.
    ///
    /// [`SupervisorBuilder::try_build`](crate::SupervisorBuilder::try_build) rejects values above Tokio's structural limit.
    pub const fn with_registry_queue_capacity(
        mut self,
        registry_queue_capacity: NonZeroUsize,
    ) -> Self {
        self.registry_queue_capacity = registry_queue_capacity;
        self
    }

    /// Registry management-queue capacity from a raw integer.
    ///
    /// # Errors
    ///
    /// - [`ConfigError::Zero`] when `registry_queue_capacity` is zero;
    /// - [`ConfigError::TooLarge`] when `registry_queue_capacity` exceeds Tokio's structural limit.
    pub fn try_with_registry_queue_capacity(
        self,
        registry_queue_capacity: usize,
    ) -> Result<Self, ConfigError> {
        let value = checked_async_capacity("registry_queue_capacity", registry_queue_capacity)?;
        Ok(self.with_registry_queue_capacity(value))
    }

    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        if let Some(max_concurrent) = self.max_concurrent {
            validate_async_capacity("max_concurrent", max_concurrent)?;
        }
        validate_async_capacity("bus_capacity", self.bus_capacity)?;
        validate_async_capacity("registry_queue_capacity", self.registry_queue_capacity)
    }
}

impl Default for SupervisorConfig {
    /// Built-in runtime configuration.
    ///
    /// Defaults:
    ///
    /// - graceful task shutdown: 60 seconds,
    /// - subscriber drain: 5 seconds,
    /// - task-attempt concurrency: unlimited,
    /// - registered-task membership: 1024,
    /// - owned user lifetimes: 1024,
    /// - event bus capacity: 1024,
    /// - registry command capacity: 1024.
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_contract_is_explicit() {
        const CONFIG: SupervisorConfig = SupervisorConfig::new();
        const LIMITED: SupervisorConfig =
            SupervisorConfig::new().with_max_concurrent(NonZeroUsize::new(4));
        let config = SupervisorConfig::default();

        assert_eq!(CONFIG.grace(), config.grace());
        assert_eq!(CONFIG.max_concurrent(), config.max_concurrent());
        assert_eq!(LIMITED.max_concurrent().map(NonZeroUsize::get), Some(4));
        assert_eq!(config.grace(), Duration::from_secs(60));
        assert_eq!(config.subscriber_shutdown_timeout(), Duration::from_secs(5));
        assert_eq!(config.max_concurrent(), None);
        assert_eq!(
            config.max_registered_tasks().map(NonZeroUsize::get),
            Some(1024)
        );
        assert_eq!(
            config.ownership_capacity().map(NonZeroUsize::get),
            Some(1024)
        );
        assert_eq!(config.bus_capacity().get(), 1024);
        assert_eq!(config.registry_queue_capacity().get(), 1024);
    }

    #[test]
    fn typed_builders_preserve_runtime_invariants() {
        let config = SupervisorConfig::default()
            .with_grace(Duration::ZERO)
            .with_subscriber_shutdown_timeout(Duration::from_secs(2))
            .with_max_concurrent(NonZeroUsize::new(4))
            .with_max_registered_tasks(NonZeroUsize::new(32))
            .with_ownership_capacity(NonZeroUsize::new(64))
            .with_bus_capacity(NonZeroUsize::new(8).unwrap())
            .with_registry_queue_capacity(NonZeroUsize::new(16).unwrap());

        assert_eq!(config.grace(), Duration::ZERO);
        assert_eq!(config.subscriber_shutdown_timeout(), Duration::from_secs(2));
        assert_eq!(config.max_concurrent().map(NonZeroUsize::get), Some(4));
        assert_eq!(
            config.max_registered_tasks().map(NonZeroUsize::get),
            Some(32)
        );
        assert_eq!(config.ownership_capacity().map(NonZeroUsize::get), Some(64));
        assert_eq!(config.bus_capacity().get(), 8);
        assert_eq!(config.registry_queue_capacity().get(), 16);
    }

    #[test]
    fn grace_is_normalized_once_and_getter_returns_the_effective_value() {
        let maximum = SupervisorConfig::new().with_grace(MAX_GRACE);
        let excessive = SupervisorConfig::new().with_grace(Duration::MAX);
        let fractional_excess = SupervisorConfig::new().with_grace(Duration::new(
            MAX_GRACE.as_secs(),
            MAX_GRACE.subsec_nanos() + 1,
        ));

        assert_eq!(maximum.grace(), MAX_GRACE);
        assert_eq!(excessive.grace(), MAX_GRACE);
        assert_eq!(fractional_excess.grace(), MAX_GRACE);
    }

    #[test]
    fn raw_zero_values_return_clear_errors() {
        type RawSetter = fn(SupervisorConfig, usize) -> Result<SupervisorConfig, ConfigError>;
        let cases: [(&str, RawSetter); 5] = [
            ("max_concurrent", SupervisorConfig::try_with_max_concurrent),
            (
                "max_registered_tasks",
                SupervisorConfig::try_with_max_registered_tasks,
            ),
            (
                "ownership_capacity",
                SupervisorConfig::try_with_ownership_capacity,
            ),
            ("bus_capacity", SupervisorConfig::try_with_bus_capacity),
            (
                "registry_queue_capacity",
                SupervisorConfig::try_with_registry_queue_capacity,
            ),
        ];

        for (field, set) in cases {
            assert_eq!(
                set(SupervisorConfig::default(), 0).unwrap_err(),
                ConfigError::Zero { field }
            );
        }
    }

    #[test]
    fn ownership_capacity_can_be_disabled_explicitly() {
        let config = SupervisorConfig::default().with_ownership_capacity(None);
        assert_eq!(config.ownership_capacity(), None);
    }

    #[test]
    fn async_capacities_reject_values_above_tokio_structural_limit() {
        type RawSetter = fn(SupervisorConfig, usize) -> Result<SupervisorConfig, ConfigError>;
        let cases: [(&str, RawSetter); 3] = [
            ("max_concurrent", SupervisorConfig::try_with_max_concurrent),
            ("bus_capacity", SupervisorConfig::try_with_bus_capacity),
            (
                "registry_queue_capacity",
                SupervisorConfig::try_with_registry_queue_capacity,
            ),
        ];
        let excessive = MAX_ASYNC_CAPACITY + 1;

        for (field, set) in cases {
            assert_eq!(
                set(SupervisorConfig::default(), excessive).unwrap_err(),
                ConfigError::TooLarge {
                    field,
                    value: excessive,
                    max: MAX_ASYNC_CAPACITY,
                }
            );
        }
    }
}
