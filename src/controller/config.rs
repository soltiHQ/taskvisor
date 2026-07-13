//! # Controller queue limits
//!
//! [`ControllerConfig`] controls controller buffering:
//! - `max_slot_queue` limits FIFO `Queue` submissions waiting inside each slot,
//! - `queue_capacity` limits the controller's ordered command channel.
//!
//! These limits apply at different stages:
//!
//! ```text
//! caller
//!   |
//!   v
//! [controller command channel]  limit: queue_capacity
//!   |
//!   v
//! slot owner + [FIFO waiting items]  limit: max_slot_queue per slot
//! ```
//!
//! `Replace` may keep one next replacement even when `max_slot_queue` is zero.

use std::num::NonZeroUsize;

use crate::ConfigError;

const DEFAULT_QUEUE_CAPACITY: NonZeroUsize = NonZeroUsize::new(1024).unwrap();
const DEFAULT_MAX_SLOT_QUEUE: usize = 100;

/// Queue limits for the controller.
///
/// Passed to `SupervisorBuilder::with_controller`.
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
    /// This channel carries submissions and ID-based remove/cancel commands in
    /// one order. The same value limits controller operations that are waiting
    /// for the registry or terminal cleanup. Later commands remain in the
    /// bounded channel until capacity becomes available.
    /// When it is full:
    /// - `submit()` waits for capacity,
    /// - `submit_and_watch()` waits for capacity,
    /// - `try_submit()` and `try_submit_and_watch()` return [`ControllerError::Full`](crate::ControllerError::Full),
    /// - `remove()`, `cancel()`, and `cancel_with_timeout()` wait for capacity,
    /// - `try_remove()`, `try_cancel()`, and `try_cancel_with_timeout()` return
    ///   [`RuntimeError::CommandQueueFull`](crate::RuntimeError::CommandQueueFull).
    ///
    /// The non-zero type makes an unusable zero-capacity channel impossible to configure.
    queue_capacity: NonZeroUsize,

    /// Maximum number of FIFO `Queue` submissions waiting per slot.
    ///
    /// This counts only waiting FIFO items, not the current slot owner.
    ///
    /// When the limit is reached, new `Queue` submissions for that slot are
    /// rejected. Taskvisor also tries to emit `ControllerRejected`.
    /// A value of `0` rejects `Queue` submissions behind busy slots.
    /// `Replace` may still retain one latest replacement while the current
    /// owner is being retired.
    max_slot_queue: usize,
}

impl ControllerConfig {
    /// Creates a configuration with explicit queue limits.
    ///
    /// `queue_capacity` is non-zero by type. `max_slot_queue = 0` is valid and
    /// rejects FIFO `Queue` submissions behind a busy slot. `Replace` may still
    /// retain one latest replacement.
    pub const fn new(queue_capacity: NonZeroUsize, max_slot_queue: usize) -> Self {
        Self {
            queue_capacity,
            max_slot_queue,
        }
    }

    /// Creates a controller configuration from a raw command-queue capacity.
    ///
    /// # Errors
    /// Returns [`ConfigError::Zero`] when `queue_capacity` is zero.
    pub fn try_new(queue_capacity: usize, max_slot_queue: usize) -> Result<Self, ConfigError> {
        let queue_capacity = NonZeroUsize::new(queue_capacity).ok_or(ConfigError::Zero {
            field: "controller_queue_capacity",
        })?;
        Ok(Self::new(queue_capacity, max_slot_queue))
    }

    /// Returns the capacity of the ordered controller command channel.
    #[must_use]
    pub const fn queue_capacity(&self) -> NonZeroUsize {
        self.queue_capacity
    }

    /// Returns the maximum number of FIFO `Queue` submissions allowed per busy slot.
    ///
    /// `0` rejects `Queue` submissions behind a busy slot. `Replace` may still
    /// retain one latest replacement.
    #[must_use]
    pub const fn max_slot_queue(&self) -> usize {
        self.max_slot_queue
    }

    /// Sets the capacity of the ordered controller command channel.
    pub const fn with_queue_capacity(mut self, queue_capacity: NonZeroUsize) -> Self {
        self.queue_capacity = queue_capacity;
        self
    }

    /// Convenience setter that validates a raw command-queue capacity.
    ///
    /// # Errors
    /// Returns [`ConfigError::Zero`] when `queue_capacity` is zero.
    pub fn try_with_queue_capacity(self, queue_capacity: usize) -> Result<Self, ConfigError> {
        let queue_capacity = NonZeroUsize::new(queue_capacity).ok_or(ConfigError::Zero {
            field: "controller_queue_capacity",
        })?;
        Ok(self.with_queue_capacity(queue_capacity))
    }

    /// Sets the maximum number of FIFO `Queue` submissions allowed per busy slot.
    ///
    /// `0` rejects `Queue` submissions behind a busy slot. `Replace` may still
    /// retain one latest replacement.
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
