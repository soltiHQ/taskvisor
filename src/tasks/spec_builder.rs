use crate::{BackoffPolicy, RestartPolicy, TaskError, TaskFn, TaskRef, TaskSpec};
use std::borrow::Cow;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Builder for TaskSpec with fluent API
#[derive(Clone)]
pub struct TaskSpecBuilder {
    name: Cow<'static, str>,
    restart: RestartPolicy,
    backoff: BackoffPolicy,
    timeout: Option<Duration>,
}

impl TaskSpecBuilder {
    /// Creates a new builder with the given task name
    pub fn new(name: impl Into<Cow<'static, str>>) -> Self {
        Self {
            name: name.into(),
            restart: RestartPolicy::default(),
            backoff: BackoffPolicy::default(),
            timeout: None,
        }
    }

    pub fn with_restart(mut self, restart: RestartPolicy) -> Self {
        self.restart = restart;
        self
    }

    pub fn with_backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = backoff;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Build TaskSpec from a closure
    pub fn build<F, Fut>(self, f: F) -> TaskSpec
    where
        F: Fn(CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), TaskError>> + Send + 'static,
    {
        let task = TaskFn::arc(self.name, f);
        TaskSpec::new(task, self.restart, self.backoff, self.timeout)
    }

    /// Build TaskSpec from an existing TaskRef
    pub fn build_from_task(self, task: TaskRef) -> TaskSpec {
        TaskSpec::new(task, self.restart, self.backoff, self.timeout)
    }
}

impl TaskSpec {
    /// Creates a builder for constructing TaskSpec with fluent API
    pub fn builder(name: impl Into<Cow<'static, str>>) -> TaskSpecBuilder {
        TaskSpecBuilder::new(name)
    }
}
