//! Controller configuration.
//!
//! [`ControllerConfig`] controls controller buffering:
//! - `max_slot_queue` limits pending submissions inside each slot,
//! - `queue_capacity` limits the controller's ordered command channel.
//!
//! These are different queues.
//! `queue_capacity` is about getting a submission or identity-removal command into the controller.
//! `max_slot_queue` is about how many accepted submissions may wait behind a busy slot.

/// Configuration for the controller.
///
/// Passed to `SupervisorBuilder::with_controller`.
///
/// # Example
///
/// ```rust
/// use taskvisor::ControllerConfig;
///
/// let config = ControllerConfig {
///     queue_capacity: 1024,
///     max_slot_queue: 100,
/// };
/// ```
#[derive(Clone, Debug)]
pub struct ControllerConfig {
    /// Capacity of the ordered controller command channel.
    ///
    /// This queue carries submissions and identity-removal commands in one order.
    /// The same value limits controller-owned identity operations that are waiting on the registry
    /// or terminal task cleanup. When that limit is reached, the controller leaves later commands
    /// in this bounded queue until an operation finishes.
    /// When it is full:
    /// - `submit()` waits for capacity,
    /// - `submit_and_watch()` waits for capacity,
    /// - `try_submit()` returns [`ControllerError::Full`](crate::ControllerError::Full).
    /// - `remove()`, `cancel()`, and `cancel_with_timeout()` wait for capacity,
    /// - `try_remove()` returns [`RuntimeError::CommandQueueFull`](crate::RuntimeError::CommandQueueFull).
    ///
    /// A value of `0` is clamped to `1`.
    pub queue_capacity: usize,

    /// Maximum number of pending submissions per slot.
    ///
    /// This is the queue behind a busy slot.
    /// It does not include the current slot owner.
    ///
    /// When the limit is reached, new `Queue` submissions for that slot are rejected with `ControllerRejected`.
    /// The current owner may still run, but no extra submission may wait behind it.
    /// A value of `0` disables queueing behind busy slots.
    pub max_slot_queue: usize,
}

impl Default for ControllerConfig {
    /// Returns the default controller configuration.
    ///
    /// Defaults:
    /// - `queue_capacity = 1024`
    /// - `max_slot_queue = 100`
    fn default() -> Self {
        Self {
            queue_capacity: 1024,
            max_slot_queue: 100,
        }
    }
}
