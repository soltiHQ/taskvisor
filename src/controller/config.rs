//! # Controller queue limits
//!
//! [`ControllerConfig`] controls controller buffering and resource budgets:
//! - `queue_capacity` limits only the ordered public command channel;
//! - `admission_capacity` limits submissions concurrently waiting for transient registry capacity;
//! - `identity_operation_capacity` limits registry-backed remove/cancel workers;
//! - `max_slot_queue` limits one busy slot's pending queue;
//! - `max_controller_slots` and `max_total_pending` bound aggregate controller state unless explicitly disabled.
//!
//! The limits apply at different boundaries:
//!
//! | Setting          | Scope                      | Rule                                                                             |
//! |------------------|----------------------------|----------------------------------------------------------------------------------|
//! | `queue_capacity`            | public command channel       | Bound accepted commands waiting for the controller loop.              |
//! | `admission_capacity`        | registry-capacity waiters    | Reject a new transient waiter when the budget is exhausted.           |
//! | `identity_operation_capacity` | registry identity workers | Cap concurrently executing registry-backed remove/cancel operations; excess fallbacks return a resource-limit error.  |
//! | `max_slot_queue`            | each busy slot               | Reject a new `Queue` when pending depth is at or above the limit.      |
//! | `max_controller_slots`      | all controller slots         | Optionally reject creation of another distinct slot.                  |
//! | `max_total_pending`         | all controller-owned pending | Optionally cap metadata, queued, and registry-capacity-waiting submissions. |

use std::num::NonZeroUsize;

use crate::{ConfigError, core::validate_async_capacity};

const DEFAULT_QUEUE_CAPACITY: NonZeroUsize = NonZeroUsize::new(1024).unwrap();
const DEFAULT_MAX_SLOT_QUEUE: usize = 100;

/// Queue limits for the controller.
///
/// Pass this value to [`SupervisorBuilder::with_controller`](crate::SupervisorBuilder::with_controller).
///
/// # Example
///
/// ```rust
/// use std::num::NonZeroUsize;
/// use taskvisor::ControllerConfig;
///
/// let config = ControllerConfig::new(NonZeroUsize::new(1024).unwrap(), 100);
/// assert_eq!(config.queue_capacity().get(), 1024);
/// assert_eq!(config.max_slot_queue(), 100);
/// ```
#[derive(Clone, Debug)]
#[must_use]
pub struct ControllerConfig {
    /// Capacity of the ordered controller command channel.
    ///
    /// This channel receives submissions and ID-based remove/cancel commands in order.
    /// Queued-work checks happen in that order.
    /// If an ID is not queued, its registry-backed operation may finish concurrently with later operations.
    /// When the command channel is full:
    /// - async `submit()` and `submit_and_watch()` methods wait for capacity,
    /// - `try_submit()` and `try_submit_and_watch()` methods return [`ControllerError::Full`](crate::ControllerError::Full),
    /// - `remove()`, `cancel()`, and `cancel_with_timeout()` wait for capacity,
    /// - `try_remove()`, `try_cancel()`, and `try_cancel_with_timeout()` return [`RuntimeError::CommandQueueFull`](crate::RuntimeError::CommandQueueFull).
    ///
    /// The non-zero type makes an unusable zero-capacity channel impossible to configure.
    queue_capacity: NonZeroUsize,

    /// Maximum number of submissions waiting for transient registry command capacity.
    ///
    /// One central admission pump owns at most one registry reservation future and a bounded FIFO of remaining identities.
    admission_capacity: NonZeroUsize,

    /// Maximum number of concurrent registry-backed remove/cancel operations.
    ///
    /// Controller-local queued removal is handled before this limit. A registry fallback received
    /// while all worker slots are occupied returns [`RuntimeError::ResourceLimitReached`](crate::RuntimeError::ResourceLimitReached)
    /// without blocking later controller commands.
    identity_operation_capacity: NonZeroUsize,

    /// Admission threshold for new FIFO `Queue` submissions in one busy slot.
    ///
    /// The current owner is not counted.
    /// All pending entries are counted, including a replacement at the queue head.
    /// A new `Queue` submission is rejected when this pending depth is already greater than or equal to the limit.
    /// The controller also publishes a best-effort [`ControllerRejected`](crate::EventKind::ControllerRejected) event.
    ///
    /// A value of `0` rejects every `Queue` submission behind a busy slot.
    /// `Replace` may still create or replace the head because it does not use this check.
    max_slot_queue: usize,

    /// Optional aggregate limit for distinct controller slots.
    max_controller_slots: Option<NonZeroUsize>,

    /// Optional aggregate limit for queued and registry-capacity-waiting submissions.
    max_total_pending: Option<NonZeroUsize>,
}

impl ControllerConfig {
    /// Creates a configuration with explicit queue limits.
    ///
    /// `queue_capacity` is non-zero by type.
    /// [`SupervisorBuilder::try_build`](crate::SupervisorBuilder::try_build)
    /// rejects values above the bounded async implementation limit.
    /// `max_slot_queue = 0` is valid and rejects FIFO `Queue` submissions behind a busy slot.
    /// `Replace` may still create or replace the queue head.
    pub const fn new(queue_capacity: NonZeroUsize, max_slot_queue: usize) -> Self {
        Self {
            queue_capacity,
            admission_capacity: queue_capacity,
            identity_operation_capacity: queue_capacity,
            max_slot_queue,
            max_controller_slots: Some(queue_capacity),
            max_total_pending: Some(queue_capacity),
        }
    }

    /// Creates a controller configuration from a raw command-queue capacity.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `queue_capacity` is zero, or
    /// [`ConfigError::TooLarge`] above the bounded async implementation limit.
    pub fn try_new(queue_capacity: usize, max_slot_queue: usize) -> Result<Self, ConfigError> {
        let queue_capacity = NonZeroUsize::new(queue_capacity).ok_or(ConfigError::Zero {
            field: "controller_queue_capacity",
        })?;
        validate_async_capacity("controller_queue_capacity", queue_capacity)?;
        Ok(Self::new(queue_capacity, max_slot_queue))
    }

    /// Returns the command-channel capacity.
    #[must_use]
    pub const fn queue_capacity(&self) -> NonZeroUsize {
        self.queue_capacity
    }

    /// Returns the transient registry-capacity admission budget.
    #[must_use]
    pub const fn admission_capacity(&self) -> NonZeroUsize {
        self.admission_capacity
    }

    /// Returns the concurrent registry-backed identity-operation budget.
    ///
    /// When the budget is exhausted, a new registry fallback returns a resource-limit error;
    /// controller-local queued removal and later submissions remain responsive.
    #[must_use]
    pub const fn identity_operation_capacity(&self) -> NonZeroUsize {
        self.identity_operation_capacity
    }

    /// Returns the pending-depth threshold for new FIFO `Queue` submissions.
    ///
    /// The owner is not counted.
    /// A replacement head is counted, but `Replace` itself does not use this threshold.
    #[must_use]
    pub const fn max_slot_queue(&self) -> usize {
        self.max_slot_queue
    }

    /// Returns the aggregate controller-slot limit.
    ///
    /// The default equals `queue_capacity`. Set `None` explicitly to disable the limit.
    #[must_use]
    pub const fn max_controller_slots(&self) -> Option<NonZeroUsize> {
        self.max_controller_slots
    }

    /// Returns the aggregate pending-submission limit.
    ///
    /// Metadata-waiting, queued, and registry-capacity-waiting submissions count.
    /// Registry-owned running tasks do not.
    /// The default equals `queue_capacity`. Set `None` explicitly to disable the limit.
    #[must_use]
    pub const fn max_total_pending(&self) -> Option<NonZeroUsize> {
        self.max_total_pending
    }

    /// Sets only the public command-channel capacity.
    ///
    /// [`SupervisorBuilder::try_build`](crate::SupervisorBuilder::try_build)
    /// rejects values above the bounded async implementation limit.
    pub const fn with_queue_capacity(mut self, queue_capacity: NonZeroUsize) -> Self {
        self.queue_capacity = queue_capacity;
        self
    }

    /// Convenience setter that validates a raw command-queue capacity.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `queue_capacity` is zero, or
    /// [`ConfigError::TooLarge`] above the bounded async implementation limit.
    pub fn try_with_queue_capacity(self, queue_capacity: usize) -> Result<Self, ConfigError> {
        let queue_capacity = NonZeroUsize::new(queue_capacity).ok_or(ConfigError::Zero {
            field: "controller_queue_capacity",
        })?;
        validate_async_capacity("controller_queue_capacity", queue_capacity)?;
        Ok(self.with_queue_capacity(queue_capacity))
    }

    /// Sets the transient registry-capacity admission budget.
    pub const fn with_admission_capacity(mut self, admission_capacity: NonZeroUsize) -> Self {
        self.admission_capacity = admission_capacity;
        self
    }

    /// Sets the admission budget from a raw integer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `admission_capacity` is zero.
    pub fn try_with_admission_capacity(
        self,
        admission_capacity: usize,
    ) -> Result<Self, ConfigError> {
        let admission_capacity =
            NonZeroUsize::new(admission_capacity).ok_or(ConfigError::Zero {
                field: "controller_admission_capacity",
            })?;
        Ok(self.with_admission_capacity(admission_capacity))
    }

    /// Sets the concurrent registry-backed identity-operation budget.
    pub const fn with_identity_operation_capacity(
        mut self,
        identity_operation_capacity: NonZeroUsize,
    ) -> Self {
        self.identity_operation_capacity = identity_operation_capacity;
        self
    }

    /// Sets the identity-operation budget from a raw integer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `identity_operation_capacity` is zero.
    pub fn try_with_identity_operation_capacity(
        self,
        identity_operation_capacity: usize,
    ) -> Result<Self, ConfigError> {
        let identity_operation_capacity =
            NonZeroUsize::new(identity_operation_capacity).ok_or(ConfigError::Zero {
                field: "controller_identity_operation_capacity",
            })?;
        Ok(self.with_identity_operation_capacity(identity_operation_capacity))
    }

    /// Sets the pending-depth threshold for new FIFO `Queue` submissions.
    ///
    /// `0` rejects `Queue` submissions behind a busy slot.
    /// `Replace` may still create or replace the queue head.
    pub const fn with_max_slot_queue(mut self, max_slot_queue: usize) -> Self {
        self.max_slot_queue = max_slot_queue;
        self
    }

    /// Sets or clears the aggregate controller-slot limit.
    pub const fn with_max_controller_slots(
        mut self,
        max_controller_slots: Option<NonZeroUsize>,
    ) -> Self {
        self.max_controller_slots = max_controller_slots;
        self
    }

    /// Sets the aggregate controller-slot limit from a raw integer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `max_controller_slots` is zero.
    pub fn try_with_max_controller_slots(
        self,
        max_controller_slots: usize,
    ) -> Result<Self, ConfigError> {
        let value = NonZeroUsize::new(max_controller_slots).ok_or(ConfigError::Zero {
            field: "max_controller_slots",
        })?;
        Ok(self.with_max_controller_slots(Some(value)))
    }

    /// Sets or clears the aggregate pending-submission limit.
    pub const fn with_max_total_pending(mut self, max_total_pending: Option<NonZeroUsize>) -> Self {
        self.max_total_pending = max_total_pending;
        self
    }

    /// Sets the aggregate pending-submission limit from a raw integer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `max_total_pending` is zero.
    pub fn try_with_max_total_pending(self, max_total_pending: usize) -> Result<Self, ConfigError> {
        let value = NonZeroUsize::new(max_total_pending).ok_or(ConfigError::Zero {
            field: "max_total_pending",
        })?;
        Ok(self.with_max_total_pending(Some(value)))
    }

    /// Validates capacities accepted through infallible `NonZeroUsize` setters.
    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        validate_async_capacity("controller_queue_capacity", self.queue_capacity)
    }
}

impl Default for ControllerConfig {
    /// Returns the default controller configuration.
    ///
    /// Defaults:
    /// - `queue_capacity = 1024`
    /// - `admission_capacity = 1024`
    /// - `identity_operation_capacity = 1024`
    /// - `max_slot_queue = 100`
    /// - `max_controller_slots = 1024`
    /// - `max_total_pending = 1024`
    fn default() -> Self {
        Self::new(DEFAULT_QUEUE_CAPACITY, DEFAULT_MAX_SLOT_QUEUE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_contract_is_explicit() {
        let config = ControllerConfig::default();
        assert_eq!(config.queue_capacity().get(), 1024);
        assert_eq!(config.admission_capacity().get(), 1024);
        assert_eq!(config.identity_operation_capacity().get(), 1024);
        assert_eq!(config.max_slot_queue(), 100);
        assert_eq!(
            config.max_controller_slots().map(NonZeroUsize::get),
            Some(1024)
        );
        assert_eq!(
            config.max_total_pending().map(NonZeroUsize::get),
            Some(1024)
        );
    }

    #[test]
    fn constructor_and_builders_preserve_invariants() {
        let config = ControllerConfig::new(NonZeroUsize::new(8).unwrap(), 3)
            .with_queue_capacity(NonZeroUsize::new(16).unwrap())
            .with_admission_capacity(NonZeroUsize::new(4).unwrap())
            .with_identity_operation_capacity(NonZeroUsize::new(2).unwrap())
            .with_max_controller_slots(NonZeroUsize::new(32))
            .with_max_total_pending(NonZeroUsize::new(64))
            .with_max_slot_queue(0);

        assert_eq!(config.queue_capacity().get(), 16);
        assert_eq!(config.admission_capacity().get(), 4);
        assert_eq!(config.identity_operation_capacity().get(), 2);
        assert_eq!(config.max_slot_queue(), 0);
        assert_eq!(
            config.max_controller_slots().map(NonZeroUsize::get),
            Some(32)
        );
        assert_eq!(config.max_total_pending().map(NonZeroUsize::get), Some(64));
    }

    #[test]
    fn raw_zero_capacity_returns_a_clear_error() {
        for result in [
            ControllerConfig::try_new(0, 10),
            ControllerConfig::default().try_with_queue_capacity(0),
        ] {
            assert_eq!(
                result.unwrap_err(),
                ConfigError::Zero {
                    field: "controller_queue_capacity"
                }
            );
        }

        for (result, field) in [
            (
                ControllerConfig::default().try_with_admission_capacity(0),
                "controller_admission_capacity",
            ),
            (
                ControllerConfig::default().try_with_identity_operation_capacity(0),
                "controller_identity_operation_capacity",
            ),
            (
                ControllerConfig::default().try_with_max_controller_slots(0),
                "max_controller_slots",
            ),
            (
                ControllerConfig::default().try_with_max_total_pending(0),
                "max_total_pending",
            ),
        ] {
            assert_eq!(result.unwrap_err(), ConfigError::Zero { field });
        }
    }

    #[test]
    fn command_capacity_rejects_values_above_tokio_structural_limit() {
        let excessive = crate::core::MAX_ASYNC_CAPACITY + 1;
        for result in [
            ControllerConfig::try_new(excessive, 10),
            ControllerConfig::default().try_with_queue_capacity(excessive),
        ] {
            assert_eq!(
                result.unwrap_err(),
                ConfigError::TooLarge {
                    field: "controller_queue_capacity",
                    value: excessive,
                    max: crate::core::MAX_ASYNC_CAPACITY,
                }
            );
        }
    }
}
