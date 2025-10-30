//! # Custom Subscriber Example
//!
//! Shows how to implement a custom event subscriber to track task metrics.
//!
//! The example counts:
//! - Total task starts
//! - Successful completions
//! - Failures
//!
//! ## Run
//! ```bash
//! cargo run --example subscriber
//! ```

use std::{
    sync::Arc,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
    time::Duration,
};
use taskvisor::{
    BackoffPolicy, Config, Event, EventKind, RestartPolicy, Subscribe, Supervisor, TaskError,
    TaskFn, TaskRef, TaskSpec,
};
use tokio_util::sync::CancellationToken;

struct MetricsSubscriber {
    starts: AtomicU64,
    failures: AtomicU64,
    successes: AtomicU64,
}

impl MetricsSubscriber {
    fn new() -> Self {
        Self {
            starts: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            successes: AtomicU64::new(0),
        }
    }
    fn print_stats(&self) {
        println!();
        println!("Metrics:");
        println!(" ├─► Starts:    {}", self.starts.load(Ordering::Relaxed));
        println!(" ├─► Failures:  {}", self.failures.load(Ordering::Relaxed));
        println!(" └─► Successes: {}", self.successes.load(Ordering::Relaxed));
    }
}

#[async_trait::async_trait]
impl Subscribe for MetricsSubscriber {
    async fn on_event(&self, ev: &Event) {
        match ev.kind {
            EventKind::TaskStarting => {
                self.starts.fetch_add(1, Ordering::Relaxed);
            }
            EventKind::TaskStopped => {
                self.successes.fetch_add(1, Ordering::Relaxed);
            }
            EventKind::TaskFailed => {
                self.failures.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
    fn name(&self) -> &'static str {
        "metrics"
    }
    fn queue_capacity(&self) -> usize {
        1024
    }
}

fn make_spec() -> TaskSpec {
    let counter = Arc::new(AtomicU32::new(0));

    let task: TaskRef = TaskFn::arc("flaky", move |ctx: CancellationToken| {
        let counter = Arc::clone(&counter);
        async move {
            if ctx.is_cancelled() {
                return Err(TaskError::Canceled);
            }

            let attempt = counter.fetch_add(1, Ordering::Relaxed) + 1;
            tokio::time::sleep(Duration::from_millis(100)).await;

            if attempt <= 4 {
                return Err(TaskError::Fail {
                    reason: format!("attempt {attempt} failed"),
                });
            }
            Ok(())
        }
    });
    TaskSpec::new(
        task,
        RestartPolicy::OnFailure,
        BackoffPolicy::default(),
        None,
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let metrics = Arc::new(MetricsSubscriber::new());

    let subs: Vec<Arc<dyn Subscribe>> = vec![Arc::clone(&metrics) as Arc<dyn Subscribe>];
    let sup = Supervisor::new(Config::default(), subs);

    sup.run(vec![make_spec()]).await?;
    metrics.print_stats();
    Ok(())
}
