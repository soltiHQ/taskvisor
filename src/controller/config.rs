//! # Configuration for the [`Controller`](super::Controller).

/// Configuration for the controller.
///
/// # Also
///
/// - `SupervisorBuilder::with_controller` - where config is wired in
/// - [`ControllerSpec`](super::ControllerSpec) - per-submission parameters
/// - `Controller` - consumes this config
#[derive(Clone, Debug)]
pub struct ControllerConfig {
    /// Capacity of the submission queue.
    ///
    /// When full, `submit()` will wait and `try_submit()` will return `Full` error.
    pub queue_capacity: usize,

    /// Maximum number of pending tasks per slot (per-slot queue depth limit).
    ///
    /// When exceeded, new submissions are rejected with `ControllerRejected` event.
    pub max_slot_queue: usize,
}

impl Default for ControllerConfig {
    /// Default configuration:
    ///
    /// - `queue_capacity = 1024`
    /// - `max_slot_queue = 100`
    fn default() -> Self {
        Self {
            queue_capacity: 1024,
            max_slot_queue: 100,
        }
    }
}
