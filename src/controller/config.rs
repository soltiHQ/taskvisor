//! # Controller queue limits
//!
//! [`ControllerConfig`] controls controller buffering:
//! - `queue_capacity` limits the ordered command channel. The same value separately caps remove/cancel operations that did not find queued work and are waiting on the runtime registry;
//! - `max_slot_queue` controls whether a new `Queue` submission may join a busy slot's pending queue.
//!
//! The limits apply at different boundaries:
//!
//! | Setting          | Scope                      | Rule                                                                             |
//! |------------------|----------------------------|----------------------------------------------------------------------------------|
//! | `queue_capacity` | commands and remove/cancel | Set the command buffer; separately cap registry-backed remove/cancel operations. |
//! | `max_slot_queue` | each busy slot             | Reject a new `Queue` when pending depth is at or above the limit.                |

use std::num::NonZeroUsize;

use crate::ConfigError;

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
    /// The same value separately caps those registry-backed operations.
    /// When that cap is reached, the controller stops draining new commands; later commands remain in the bounded channel.
    ///
    /// When the command channel is full:
    /// - `submit()` waits for capacity,
    /// - `submit_and_watch()` waits for capacity,
    /// - `try_submit()` and `try_submit_and_watch()` return [`ControllerError::Full`](crate::ControllerError::Full),
    /// - `remove()`, `cancel()`, and `cancel_with_timeout()` wait for capacity,
    /// - `try_remove()`, `try_cancel()`, and `try_cancel_with_timeout()` return [`RuntimeError::CommandQueueFull`](crate::RuntimeError::CommandQueueFull).
    ///
    /// The non-zero type makes an unusable zero-capacity channel impossible to configure.
    queue_capacity: NonZeroUsize,

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
}

impl ControllerConfig {
    /// Creates a configuration with explicit queue limits.
    ///
    /// `queue_capacity` is non-zero by type.
    /// `max_slot_queue = 0` is valid and rejects FIFO `Queue` submissions behind a busy slot.
    /// `Replace` may still create or replace the queue head.
    pub const fn new(queue_capacity: NonZeroUsize, max_slot_queue: usize) -> Self {
        Self {
            queue_capacity,
            max_slot_queue,
        }
    }

    /// Creates a controller configuration from a raw command-queue capacity.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `queue_capacity` is zero.
    pub fn try_new(queue_capacity: usize, max_slot_queue: usize) -> Result<Self, ConfigError> {
        let queue_capacity = NonZeroUsize::new(queue_capacity).ok_or(ConfigError::Zero {
            field: "controller_queue_capacity",
        })?;
        Ok(Self::new(queue_capacity, max_slot_queue))
    }

    /// Returns the command-channel capacity and the separate cap for registry-backed remove/cancel operations.
    #[must_use]
    pub const fn queue_capacity(&self) -> NonZeroUsize {
        self.queue_capacity
    }

    /// Returns the pending-depth threshold for new FIFO `Queue` submissions.
    ///
    /// The owner is not counted.
    /// A replacement head is counted, but `Replace` itself does not use this threshold.
    #[must_use]
    pub const fn max_slot_queue(&self) -> usize {
        self.max_slot_queue
    }

    /// Sets the command-channel capacity and the separate cap for registry-backed remove/cancel operations.
    pub const fn with_queue_capacity(mut self, queue_capacity: NonZeroUsize) -> Self {
        self.queue_capacity = queue_capacity;
        self
    }

    /// Convenience setter that validates a raw command-queue capacity.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `queue_capacity` is zero.
    pub fn try_with_queue_capacity(self, queue_capacity: usize) -> Result<Self, ConfigError> {
        let queue_capacity = NonZeroUsize::new(queue_capacity).ok_or(ConfigError::Zero {
            field: "controller_queue_capacity",
        })?;
        Ok(self.with_queue_capacity(queue_capacity))
    }

    /// Sets the pending-depth threshold for new FIFO `Queue` submissions.
    ///
    /// `0` rejects `Queue` submissions behind a busy slot.
    /// `Replace` may still create or replace the queue head.
    pub const fn with_max_slot_queue(mut self, max_slot_queue: usize) -> Self {
        self.max_slot_queue = max_slot_queue;
        self
    }
}

impl Default for ControllerConfig {
    /// Returns the default controller configuration.
    ///
    /// Defaults:
    /// - `queue_capacity = 1024`
    /// - `max_slot_queue = 100`
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
        assert_eq!(config.max_slot_queue(), 100);
    }

    #[test]
    fn constructor_and_builders_preserve_invariants() {
        let config = ControllerConfig::new(NonZeroUsize::new(8).unwrap(), 3)
            .with_queue_capacity(NonZeroUsize::new(16).unwrap())
            .with_max_slot_queue(0);

        assert_eq!(config.queue_capacity().get(), 16);
        assert_eq!(config.max_slot_queue(), 0);
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
    }
}
