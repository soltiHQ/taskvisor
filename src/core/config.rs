//! Runtime-wide limits and shutdown settings.
//!
//! Per-task restart, backoff, timeout, and retry defaults live in [`TaskDefaults`](crate::TaskDefaults).

use std::num::NonZeroUsize;
use std::time::Duration;

use thiserror::Error;

/// Default capacity of the event bus and registry command queue.
const DEFAULT_CAPACITY: NonZeroUsize = NonZeroUsize::new(1024).unwrap();

/// Default deadline for draining subscriber queues during shutdown.
const DEFAULT_SUBSCRIBER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Structural upper bound shared by Tokio bounded channels and semaphores.
pub(crate) const MAX_ASYNC_CAPACITY: usize = tokio::sync::Semaphore::MAX_PERMITS;

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

/// Largest effective task-stop window accepted by the runtime.
///
/// Bounding this once at configuration time keeps Tokio deadline arithmetic
/// valid and makes getters, timeout behaviour, and diagnostics agree.
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
    /// A bounded async capacity exceeds the implementation's structural maximum.
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

/// Runtime-wide settings for one supervisor.
///
/// | Setting                       | What it controls                                    |
/// |-------------------------------|-----------------------------------------------------|
/// | `grace`                       | Time allowed for cooperative task stop before logical force-abort |
/// | `subscriber_shutdown_timeout` | Time allowed to drain subscriber queues             |
/// | `max_concurrent`              | Number of task attempts that may run at once        |
/// | `max_registered_tasks`        | Registry identities plus attempts still being reaped |
/// | `bus_capacity`                | Number of newest events kept by event ingress       |
/// | `registry_queue_capacity`     | Capacity of the bounded registry command queue      |
///
/// Backoff sleeps do not use a `max_concurrent` permit.
/// The event bus is best-effort even with a large capacity; slow consumers can still miss events.
/// The subscriber deadline can drop queued events, but it cannot stop a callback that is already running.
///
/// Direct state-changing `add*`, `remove*`, and `cancel*` commands, static batch registration, and controller-to-registry work all use the registry command queue.
///
/// Configure task execution defaults with [`TaskDefaults`](crate::TaskDefaults) through [`SupervisorBuilder::with_task_defaults`](crate::SupervisorBuilder::with_task_defaults).
#[derive(Clone, Debug)]
#[must_use]
pub struct SupervisorConfig {
    grace: Duration,
    subscriber_shutdown_timeout: Duration,
    max_concurrent: Option<NonZeroUsize>,
    max_registered_tasks: Option<NonZeroUsize>,
    bus_capacity: NonZeroUsize,
    registry_queue_capacity: NonZeroUsize,
}

impl SupervisorConfig {
    /// Creates the default configuration in a const context.
    ///
    /// This has the same values as [`Default::default`].
    /// The explicit constructor makes the `const` getters and setters usable for compile-time configuration.
    pub const fn new() -> Self {
        Self {
            grace: Duration::from_secs(60),
            subscriber_shutdown_timeout: DEFAULT_SUBSCRIBER_SHUTDOWN_TIMEOUT,
            max_concurrent: None,
            max_registered_tasks: Some(DEFAULT_CAPACITY),
            bus_capacity: DEFAULT_CAPACITY,
            registry_queue_capacity: DEFAULT_CAPACITY,
        }
    }

    /// Returns the cooperative task-stop window.
    ///
    /// It is used for explicit removal and runtime shutdown.
    /// After this period, Taskvisor commits a logical force-abort and transfers
    /// any still-running actor to reaper ownership. Zero means no graceful wait.
    #[must_use]
    pub const fn grace(&self) -> Duration {
        self.grace
    }

    /// Returns the shared deadline for draining subscriber queues.
    ///
    /// Zero closes the queues without waiting for pending events.
    /// The deadline can drop queued events, but it cannot stop a callback already running.
    #[must_use]
    pub const fn subscriber_shutdown_timeout(&self) -> Duration {
        self.subscriber_shutdown_timeout
    }

    /// Returns the global limit for running task attempts.
    ///
    /// `None` means no limit.
    /// Waiting for a permit and retry backoff do not hold one.
    /// Once an attempt starts, all work and awaits inside it hold the permit.
    #[must_use]
    pub const fn max_concurrent(&self) -> Option<NonZeroUsize> {
        self.max_concurrent
    }

    /// Returns the registry membership limit.
    ///
    /// Registered and removing tasks count until terminal cleanup removes their identity.
    /// A force-aborted attempt still being physically reaped also counts. During the handoff from
    /// registry membership to the reaper, the same task may be counted conservatively in both places.
    /// The default is `1024`. Set `None` explicitly to disable the limit.
    /// `None` disables only this supervisor's registry-membership limit; the
    /// process-wide `owned_user_lifetimes` budget of 1024 tasks and subscribers still applies.
    #[must_use]
    pub const fn max_registered_tasks(&self) -> Option<NonZeroUsize> {
        self.max_registered_tasks
    }

    /// Returns the number of newest events retained by the event ingress.
    #[must_use]
    pub const fn bus_capacity(&self) -> NonZeroUsize {
        self.bus_capacity
    }

    /// Returns the capacity of the registry management queue.
    #[must_use]
    pub const fn registry_queue_capacity(&self) -> NonZeroUsize {
        self.registry_queue_capacity
    }

    /// Sets the cooperative task-stop window before logical force-abort.
    ///
    /// Values above 30 years are normalized to 30 years. [`grace`](Self::grace)
    /// always returns the effective value used by timeout logic and diagnostics.
    pub const fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = normalize_grace(grace);
        self
    }

    /// Sets the shared deadline for draining subscriber queues.
    ///
    /// The deadline does not interrupt a subscriber callback already running.
    pub const fn with_subscriber_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.subscriber_shutdown_timeout = timeout;
        self
    }

    /// Sets or clears the global limit for running task attempts.
    ///
    /// This const method accepts `Option<NonZeroUsize>`.
    /// [`SupervisorBuilder::try_build`](crate::SupervisorBuilder::try_build)
    /// rejects values above the bounded async implementation limit.
    /// Use [`try_with_max_concurrent`](Self::try_with_max_concurrent) for a raw integer.
    pub const fn with_max_concurrent(mut self, max_concurrent: Option<NonZeroUsize>) -> Self {
        self.max_concurrent = max_concurrent;
        self
    }

    /// Sets the concurrency limit from a raw integer.
    ///
    /// # Errors
    /// Returns [`ConfigError::Zero`] when `max_concurrent` is zero, or
    /// [`ConfigError::TooLarge`] above the bounded async implementation limit.
    pub fn try_with_max_concurrent(self, max_concurrent: usize) -> Result<Self, ConfigError> {
        let value = checked_async_capacity("max_concurrent", max_concurrent)?;
        Ok(self.with_max_concurrent(Some(value)))
    }

    /// Sets or clears the registry membership limit.
    ///
    /// `None` disables only this per-supervisor limit. It does not disable the
    /// process-wide `owned_user_lifetimes` budget shared by tasks and subscribers.
    ///
    /// This const method accepts `Option<NonZeroUsize>`.
    /// Use [`try_with_max_registered_tasks`](Self::try_with_max_registered_tasks) for a raw integer.
    pub const fn with_max_registered_tasks(
        mut self,
        max_registered_tasks: Option<NonZeroUsize>,
    ) -> Self {
        self.max_registered_tasks = max_registered_tasks;
        self
    }

    /// Sets the registry membership limit from a raw integer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `max_registered_tasks` is zero.
    pub fn try_with_max_registered_tasks(
        self,
        max_registered_tasks: usize,
    ) -> Result<Self, ConfigError> {
        let value = NonZeroUsize::new(max_registered_tasks).ok_or(ConfigError::Zero {
            field: "max_registered_tasks",
        })?;
        Ok(self.with_max_registered_tasks(Some(value)))
    }

    /// Sets how many newest events the event ingress retains.
    ///
    /// [`SupervisorBuilder::try_build`](crate::SupervisorBuilder::try_build)
    /// rejects values above the bounded async implementation limit.
    pub const fn with_bus_capacity(mut self, bus_capacity: NonZeroUsize) -> Self {
        self.bus_capacity = bus_capacity;
        self
    }

    /// Sets the event-bus capacity from a raw integer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `bus_capacity` is zero, or
    /// [`ConfigError::TooLarge`] above the bounded async implementation limit.
    pub fn try_with_bus_capacity(self, bus_capacity: usize) -> Result<Self, ConfigError> {
        let value = checked_async_capacity("bus_capacity", bus_capacity)?;
        Ok(self.with_bus_capacity(value))
    }

    /// Sets the capacity of the registry management queue.
    ///
    /// [`SupervisorBuilder::try_build`](crate::SupervisorBuilder::try_build)
    /// rejects values above the bounded async implementation limit.
    pub const fn with_registry_queue_capacity(
        mut self,
        registry_queue_capacity: NonZeroUsize,
    ) -> Self {
        self.registry_queue_capacity = registry_queue_capacity;
        self
    }

    /// Sets the registry queue capacity from a raw integer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `registry_queue_capacity` is zero, or
    /// [`ConfigError::TooLarge`] above the bounded async implementation limit.
    pub fn try_with_registry_queue_capacity(
        self,
        registry_queue_capacity: usize,
    ) -> Result<Self, ConfigError> {
        let value = checked_async_capacity("registry_queue_capacity", registry_queue_capacity)?;
        Ok(self.with_registry_queue_capacity(value))
    }

    /// Validates capacities accepted through infallible `NonZeroUsize` setters.
    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        if let Some(max_concurrent) = self.max_concurrent {
            validate_async_capacity("max_concurrent", max_concurrent)?;
        }
        validate_async_capacity("bus_capacity", self.bus_capacity)?;
        validate_async_capacity("registry_queue_capacity", self.registry_queue_capacity)
    }
}

impl Default for SupervisorConfig {
    /// Returns the default runtime configuration.
    ///
    /// Defaults:
    /// - graceful task shutdown: 60 seconds,
    /// - subscriber drain: 5 seconds,
    /// - task-attempt concurrency: unlimited,
    /// - registered-task membership: 1024,
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
            .with_bus_capacity(NonZeroUsize::new(8).unwrap())
            .with_registry_queue_capacity(NonZeroUsize::new(16).unwrap());

        assert_eq!(config.grace(), Duration::ZERO);
        assert_eq!(config.subscriber_shutdown_timeout(), Duration::from_secs(2));
        assert_eq!(config.max_concurrent().map(NonZeroUsize::get), Some(4));
        assert_eq!(
            config.max_registered_tasks().map(NonZeroUsize::get),
            Some(32)
        );
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
        let cases: [(&str, RawSetter); 4] = [
            ("max_concurrent", SupervisorConfig::try_with_max_concurrent),
            (
                "max_registered_tasks",
                SupervisorConfig::try_with_max_registered_tasks,
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
