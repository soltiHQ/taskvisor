//! Controller configuration.
//!
//! [`ControllerConfig`] controls controller buffering:
//! - `max_slot_queue` limits pending submissions inside each slot,
//! - `queue_capacity` limits the controller's intake channel.
//!
//! These are different queues.
//! `queue_capacity` is about getting a submission into the controller.
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
    /// Capacity of the controller intake channel.
    ///
    /// This is the queue in front of the controller loop.
    /// When it is full:
    /// - `submit()` waits for capacity,
    /// - `submit_and_watch()` waits for capacity,
    /// - `try_submit()` returns [`ControllerError::Full`](crate::ControllerError::Full).
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
