use std::time::Duration;

use crate::{policy::RestartPolicy, strategy::BackoffStrategy};

#[derive(Clone, Debug)]
pub struct Config {
    //
    pub grace: Duration,
    //
    pub max_concurrent: usize,
    //
    pub bus_capacity: usize,
    //
    pub restart: RestartPolicy,
    //
    pub backoff: BackoffStrategy,
    //
    pub timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_concurrent: 0,
            bus_capacity: 1024,
            timeout: Duration::from_secs(0),
            grace: Duration::from_secs(30),
            restart: RestartPolicy::default(),
            backoff: BackoffStrategy::default(),
        }
    }
}
