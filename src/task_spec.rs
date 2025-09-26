use std::fmt;
use std::time::Duration;

use crate::{config::Config, policy::RestartPolicy, strategy::BackoffStrategy, task::TaskRef};

#[derive(Clone)]
pub struct TaskSpec {
    pub task: TaskRef,
    pub restart: RestartPolicy,
    pub backoff: BackoffStrategy,
    pub timeout: Option<Duration>,
}

impl TaskSpec {
    pub fn new(
        task: TaskRef,
        restart: RestartPolicy,
        backoff: BackoffStrategy,
        timeout: Option<Duration>,
    ) -> Self {
        Self {
            task,
            restart,
            backoff,
            timeout,
        }
    }

    pub fn from_task(task: TaskRef, cfg: &Config) -> Self {
        Self {
            task,
            restart: cfg.restart,
            backoff: cfg.backoff,
            timeout: (!cfg.timeout.is_zero()).then(|| cfg.timeout),
        }
    }
}

impl fmt::Debug for TaskSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskSpec")
            .field("task", &self.task.name())
            .field("restart", &self.restart)
            .field("backoff", &self.backoff)
            .field("timeout", &self.timeout)
            .finish()
    }
}
