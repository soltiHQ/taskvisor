use std::collections::VecDeque;

use crate::TaskSpec;

/// Slot state: whether something is running and what is queued.
pub struct Slot {
    pub running: bool,
    pub queue: VecDeque<TaskSpec>,
}

impl Slot {
    #[inline]
    pub fn new() -> Self {
        Self { running: false, queue: VecDeque::new() }
    }
}