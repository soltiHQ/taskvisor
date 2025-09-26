use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct BackoffStrategy {
    pub first: Duration,
    pub max: Duration,
    pub factor: f64,
}

impl Default for BackoffStrategy {
    fn default() -> Self {
        Self {
            first: Duration::from_millis(100),
            max: Duration::from_secs(30),
            factor: 1.0,
        }
    }
}

impl BackoffStrategy {
    pub fn next(&self, prev: Option<Duration>) -> Duration {
        match prev {
            None => self.first,
            Some(d) => {
                let next = (d.as_secs_f64() * self.factor).min(self.max.as_secs_f64());
                Duration::from_secs_f64(next)
            }
        }
    }
}
