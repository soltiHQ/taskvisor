#[derive(Clone)]
pub struct ControllerConfig {
    pub capacity: usize,
}

impl ControllerConfig {
    #[inline]
    pub fn default() -> ControllerConfig {
        Self { capacity: 1024 }
    }
}