//! Runtime-wide limits and shutdown settings.
//!
//! Per-task restart, backoff, timeout, and retry defaults live in
//! [`TaskDefaults`](crate::TaskDefaults).

use std::num::NonZeroUsize;
use std::time::Duration;

use thiserror::Error;

const DEFAULT_CAPACITY: NonZeroUsize = NonZeroUsize::new(1024).unwrap();
const DEFAULT_SUBSCRIBER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Error from a checked configuration setter.
///
/// Match with a wildcard arm because the enum and its data-carrying variants
/// are non-exhaustive.
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
}

/// Runtime-wide settings for one supervisor.
///
/// | Setting | What it controls |
/// |---------|------------------|
/// | `grace` | Time allowed for cooperative task stop before abort |
/// | `subscriber_shutdown_timeout` | Time allowed to drain subscriber queues |
/// | `max_concurrent` | Number of task attempts that may run at once |
/// | `bus_capacity` | Number of recent events kept by the broadcast bus |
/// | `registry_queue_capacity` | Backpressure for dynamic management commands |
///
/// Backoff sleeps do not use a `max_concurrent` permit. The event bus is
/// best-effort even with a large capacity; slow consumers can still miss events.
///
/// Configure task execution defaults with [`TaskDefaults`](crate::TaskDefaults)
/// through [`SupervisorBuilder::with_task_defaults`](crate::SupervisorBuilder::with_task_defaults).
#[derive(Clone, Debug)]
#[must_use]
pub struct SupervisorConfig {
    grace: Duration,
    subscriber_shutdown_timeout: Duration,
    max_concurrent: Option<NonZeroUsize>,
    bus_capacity: NonZeroUsize,
    registry_queue_capacity: NonZeroUsize,
}

impl SupervisorConfig {
    /// Creates the default configuration in a const context.
    ///
    /// This has the same values as [`Default::default`]. The
    /// explicit constructor makes the `const` getters and setters usable for
    /// compile-time configuration.
    pub const fn new() -> Self {
        Self {
            grace: Duration::from_secs(60),
            subscriber_shutdown_timeout: DEFAULT_SUBSCRIBER_SHUTDOWN_TIMEOUT,
            max_concurrent: None,
            bus_capacity: DEFAULT_CAPACITY,
            registry_queue_capacity: DEFAULT_CAPACITY,
        }
    }

    /// Returns the cooperative task-stop window.
    ///
    /// It is used for explicit removal and runtime shutdown. After this period,
    /// a task that is still running is aborted. Zero means no graceful wait.
    #[must_use]
    pub const fn grace(&self) -> Duration {
        self.grace
    }

    /// Returns the shared deadline for draining subscriber queues.
    ///
    /// Zero closes the queues without waiting for pending events.
    #[must_use]
    pub const fn subscriber_shutdown_timeout(&self) -> Duration {
        self.subscriber_shutdown_timeout
    }

    /// Returns the global limit for running task attempts.
    ///
    /// `None` means no limit. Waiting for a permit and retry backoff do not hold
    /// one. Once an attempt starts, all work and awaits inside it hold the permit.
    #[must_use]
    pub const fn max_concurrent(&self) -> Option<NonZeroUsize> {
        self.max_concurrent
    }

    /// Returns the number of recent events kept by the broadcast bus.
    #[must_use]
    pub const fn bus_capacity(&self) -> NonZeroUsize {
        self.bus_capacity
    }

    /// Returns the capacity of the registry management queue.
    #[must_use]
    pub const fn registry_queue_capacity(&self) -> NonZeroUsize {
        self.registry_queue_capacity
    }

    /// Sets the cooperative task-stop window before abort.
    pub const fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    /// Sets the shared deadline for draining subscriber queues.
    pub const fn with_subscriber_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.subscriber_shutdown_timeout = timeout;
        self
    }

    /// Sets or clears the global limit for running task attempts.
    ///
    /// This const method accepts `Option<NonZeroUsize>`. Use
    /// [`try_with_max_concurrent`](Self::try_with_max_concurrent) for a raw
    /// integer.
    pub const fn with_max_concurrent(mut self, max_concurrent: Option<NonZeroUsize>) -> Self {
        self.max_concurrent = max_concurrent;
        self
    }

    /// Sets the concurrency limit from a raw integer.
    ///
    /// # Errors
    /// Returns [`ConfigError::Zero`] when `max_concurrent` is zero.
    pub fn try_with_max_concurrent(self, max_concurrent: usize) -> Result<Self, ConfigError> {
        let value = NonZeroUsize::new(max_concurrent).ok_or(ConfigError::Zero {
            field: "max_concurrent",
        })?;
        Ok(self.with_max_concurrent(Some(value)))
    }

    /// Sets how many recent events the broadcast bus keeps.
    pub const fn with_bus_capacity(mut self, bus_capacity: NonZeroUsize) -> Self {
        self.bus_capacity = bus_capacity;
        self
    }

    /// Sets the event-bus capacity from a raw integer.
    ///
    /// # Errors
    /// Returns [`ConfigError::Zero`] when `bus_capacity` is zero.
    pub fn try_with_bus_capacity(self, bus_capacity: usize) -> Result<Self, ConfigError> {
        let value = NonZeroUsize::new(bus_capacity).ok_or(ConfigError::Zero {
            field: "bus_capacity",
        })?;
        Ok(self.with_bus_capacity(value))
    }

    /// Sets the capacity of the registry management queue.
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
    /// Returns [`ConfigError::Zero`] when `registry_queue_capacity` is zero.
    pub fn try_with_registry_queue_capacity(
        self,
        registry_queue_capacity: usize,
    ) -> Result<Self, ConfigError> {
        let value = NonZeroUsize::new(registry_queue_capacity).ok_or(ConfigError::Zero {
            field: "registry_queue_capacity",
        })?;
        Ok(self.with_registry_queue_capacity(value))
    }
}

impl Default for SupervisorConfig {
    /// Returns the default runtime configuration.
    ///
    /// Defaults:
    /// - graceful task shutdown: 60 seconds,
    /// - subscriber drain: 5 seconds,
    /// - task-attempt concurrency: unlimited,
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
        assert_eq!(config.bus_capacity().get(), 1024);
        assert_eq!(config.registry_queue_capacity().get(), 1024);
    }

    #[test]
    fn typed_builders_preserve_runtime_invariants() {
        let config = SupervisorConfig::default()
            .with_grace(Duration::ZERO)
            .with_subscriber_shutdown_timeout(Duration::from_secs(2))
            .with_max_concurrent(NonZeroUsize::new(4))
            .with_bus_capacity(NonZeroUsize::new(8).unwrap())
            .with_registry_queue_capacity(NonZeroUsize::new(16).unwrap());

        assert_eq!(config.grace(), Duration::ZERO);
        assert_eq!(config.subscriber_shutdown_timeout(), Duration::from_secs(2));
        assert_eq!(config.max_concurrent().map(NonZeroUsize::get), Some(4));
        assert_eq!(config.bus_capacity().get(), 8);
        assert_eq!(config.registry_queue_capacity().get(), 16);
    }

    #[test]
    fn raw_zero_values_return_clear_errors() {
        type RawSetter = fn(SupervisorConfig, usize) -> Result<SupervisorConfig, ConfigError>;
        let cases: [(&str, RawSetter); 3] = [
            ("max_concurrent", SupervisorConfig::try_with_max_concurrent),
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
}
