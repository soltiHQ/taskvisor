//! Configures bounded resources used by the controller.
//!
//! [`SupervisorBuilder::with_controller`](crate::SupervisorBuilder::with_controller)
//! installs one [`ControllerConfig`] when the supervisor is built.
//! Its limits protect distinct stages of the controller path:
//!
//! ```text
//! controller
//!      ├── command intake ────► ordered command queue
//!      ├── slot admission ────► slots and pending submissions
//!      ├── registry handoff ──► capacity waiters
//!      └── task management ───► registry-backed identity operations
//! ```
//!
//! Command-queue pressure is visible at intake. Most other admission limits are checked after intake
//! and appear as a watched rejection or best-effort event. The identity-operation limit is returned
//! by the ID-based remove or cancel call that needs registry fallback.

use std::num::NonZeroUsize;

use crate::{ConfigError, core::validate_async_capacity};

const DEFAULT_QUEUE_CAPACITY: NonZeroUsize = NonZeroUsize::new(1024).unwrap();
const DEFAULT_MAX_SLOT_QUEUE: usize = 100;

/// Resource limits for one supervisor's controller.
///
/// Pass this value to [`SupervisorBuilder::with_controller`](crate::SupervisorBuilder::with_controller)
/// before building the supervisor. [`Default`] configures every limit to a finite value and can be used without further changes.
///
/// # What each limit controls
///
/// - [`identity_operation_capacity`](Self::identity_operation_capacity) bounds concurrent registry-backed remove and cancel operations.
/// - [`max_total_pending`](Self::max_total_pending) bounds slot-queue entries plus waits for runtime registry command capacity.
/// - [`admission_capacity`](Self::admission_capacity) bounds waits for runtime registry command capacity.
/// - [`max_controller_slots`](Self::max_controller_slots) bounds distinct slots tracked at once.
/// - [`queue_capacity`](Self::queue_capacity) bounds commands waiting for the controller loop.
/// - [`max_slot_queue`](Self::max_slot_queue) bounds FIFO work behind one busy slot owner.
///
/// # How limits appear to callers
///
/// Async submit methods wait when the ordered command queue is full. Their fail-fast `try_*` forms
/// return [`ControllerError::Full`](crate::ControllerError::Full).
/// Admission limits are checked later. A watched submit reports them through [`TaskOutcome::Rejected`](crate::TaskOutcome::Rejected). A full slot queue
/// uses [`RejectionKind::QueueFull`](crate::RejectionKind::QueueFull). The admission, slot-count, and total-pending
/// budgets use [`RejectionKind::ResourceLimit`](crate::RejectionKind::ResourceLimit).
///
/// A remove or cancel call that must reach the registry returns [`RuntimeError::ResourceLimitReached`](crate::RuntimeError::ResourceLimitReached)
/// when this operation budget is exhausted. Controller-local queued removal is handled before this limit.
///
/// # Examples
///
/// ```rust
/// use std::num::NonZeroUsize;
/// use taskvisor::ControllerConfig;
///
/// let config = ControllerConfig::default()
///     .with_queue_capacity(NonZeroUsize::new(256).unwrap())
///     .with_max_slot_queue(32)
///     .with_max_controller_slots(NonZeroUsize::new(512));
///
/// assert_eq!(config.queue_capacity().get(), 256);
/// assert_eq!(config.max_slot_queue(), 32);
/// ```
#[derive(Clone, Debug)]
#[must_use]
pub struct ControllerConfig {
    /// Capacity of the ordered controller command channel.
    ///
    /// The channel orders submissions with task-ID remove and cancel commands.
    /// Async methods wait when it is full. Fail-fast methods return a queue-full error.
    queue_capacity: NonZeroUsize,

    /// Maximum active and queued reservations for registry command capacity.
    admission_capacity: NonZeroUsize,

    /// Maximum number of concurrent registry-backed remove/cancel operations.
    ///
    /// Controller-local removal of queued work is handled before this limit is checked.
    identity_operation_capacity: NonZeroUsize,

    /// Pending-depth limit checked by new `Queue` submissions in one busy slot.
    ///
    /// The owner is excluded. A replacement at the queue head is included.
    /// `Replace` does not use this per-slot limit.
    max_slot_queue: usize,

    /// Optional limit for distinct slots tracked at once.
    max_controller_slots: Option<NonZeroUsize>,

    /// Optional limit for all queued and registry-capacity-waiting submissions.
    max_total_pending: Option<NonZeroUsize>,
}

impl ControllerConfig {
    /// Creates a configuration from command capacity and per-slot queue depth.
    ///
    /// The admission budget, identity-operation budget, slot limit, and total pending limit all start at `queue_capacity`.
    /// Configuration methods can tune them independently.
    ///
    /// [`SupervisorBuilder::try_build`](crate::SupervisorBuilder::try_build) rejects a command capacity above the async implementation limit.
    /// A `max_slot_queue` value of `0` rejects every `Queue` submission behind a busy slot.
    /// `Replace` can still create or replace the queue head.
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

    /// Creates a configuration from a raw command-queue capacity.
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

    /// Returns the ordered controller command-channel capacity.
    #[must_use]
    pub const fn queue_capacity(&self) -> NonZeroUsize {
        self.queue_capacity
    }

    /// Returns the number of registry-capacity reservations allowed at once.
    #[must_use]
    pub const fn admission_capacity(&self) -> NonZeroUsize {
        self.admission_capacity
    }

    /// Returns the concurrent registry-backed remove and cancel budget.
    #[must_use]
    pub const fn identity_operation_capacity(&self) -> NonZeroUsize {
        self.identity_operation_capacity
    }

    /// Returns the per-slot pending limit checked by `Queue` submissions.
    ///
    /// The owner is excluded. A replacement head is included. `Replace` does not use this limit.
    #[must_use]
    pub const fn max_slot_queue(&self) -> usize {
        self.max_slot_queue
    }

    /// Returns the tracked-slot limit, or `None` if it is disabled.
    ///
    /// The default equals the command capacity passed to [`new`](Self::new).
    #[must_use]
    pub const fn max_controller_slots(&self) -> Option<NonZeroUsize> {
        self.max_controller_slots
    }

    /// Returns the total pending-submission limit, or `None` if disabled.
    ///
    /// Slot queues and waits for registry command capacity are included.
    /// Buffered controller commands and tasks already handed to the registry are excluded.
    /// The default equals the command capacity passed to [`new`](Self::new).
    #[must_use]
    pub const fn max_total_pending(&self) -> Option<NonZeroUsize> {
        self.max_total_pending
    }

    /// Sets the command-channel capacity without changing the other limits.
    ///
    /// [`SupervisorBuilder::try_build`](crate::SupervisorBuilder::try_build)
    /// rejects values above the bounded async implementation limit.
    pub const fn with_queue_capacity(mut self, queue_capacity: NonZeroUsize) -> Self {
        self.queue_capacity = queue_capacity;
        self
    }

    /// Sets and validates a raw command-queue capacity.
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

    /// Sets the number of registry-capacity reservations allowed at once.
    pub const fn with_admission_capacity(mut self, admission_capacity: NonZeroUsize) -> Self {
        self.admission_capacity = admission_capacity;
        self
    }

    /// Sets the registry-capacity wait budget from a raw integer.
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

    /// Sets the concurrent registry-backed remove and cancel budget.
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

    /// Sets the per-slot pending limit checked by `Queue` submissions.
    ///
    /// `0` rejects `Queue` submissions behind a busy slot.
    /// `Replace` may still create or replace the queue head.
    pub const fn with_max_slot_queue(mut self, max_slot_queue: usize) -> Self {
        self.max_slot_queue = max_slot_queue;
        self
    }

    /// Sets the tracked-slot limit, or disables it with `None`.
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

    /// Sets the total pending-submission limit, or disables it with `None`.
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
    ///
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
