/// Configuration for the controller.
#[derive(Clone, Debug)]
pub struct ControllerConfig {
    /// Capacity of the submission queue.
    ///
    /// When full, `submit()` will wait and `try_submit()` will return `Full` error.
    pub queue_capacity: usize,

    /// Capacity of the slots.
    pub slot_capacity: usize,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 1024,
            slot_capacity: 100,
        }
    }
}
