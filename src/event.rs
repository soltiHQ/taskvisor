use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    ShutdownRequested,
    BackoffScheduled,
    AllStoppedWithin,
    GraceExceeded,
    TaskStarting,
    TaskStopped,
    TaskFailed,
    TimeoutHit,
}

#[derive(Debug, Clone)]
pub struct Event {
    pub timeout: Option<Duration>,
    pub delay: Option<Duration>,
    pub error: Option<String>,
    pub attempt: Option<u64>,
    pub task: Option<String>,
    pub kind: EventKind,
    pub at: SystemTime,
}

impl Event {
    pub fn now(kind: EventKind) -> Self {
        Self {
            kind,
            at: SystemTime::now(),
            attempt: None,
            timeout: None,
            error: None,
            delay: None,
            task: None,
        }
    }

    pub fn with_error(mut self, msg: impl Into<String>) -> Self {
        self.error = Some(msg.into());
        self
    }

    pub fn with_task(mut self, name: impl Into<String>) -> Self {
        self.task = Some(name.into());
        self
    }

    pub fn with_timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }

    pub fn with_delay(mut self, d: Duration) -> Self {
        self.delay = Some(d);
        self
    }

    pub fn with_attempt(mut self, n: u64) -> Self {
        self.attempt = Some(n);
        self
    }
}
