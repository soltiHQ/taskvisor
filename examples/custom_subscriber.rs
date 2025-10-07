//! # Example: custom_subscriber
//!
//! Demonstrates how to build and attach a custom event subscriber.
//!
//! Shows how to:
//! - Implement the [`Subscribe`] trait.
//! - Inspect [`Event`] / [`EventKind`] for task lifecycle metrics.
//! - Wire the subscriber into [`Supervisor::new`].
//!
//! ## Flow
//! ```text
//! TaskSpec ──► Supervisor::run()
//!     ├─► Bus.publish(TaskAddRequested)
//!     ├─► Registry::spawn_listener()
//!     ├─► TaskActor::run()
//!     │     ├─► publish(TaskStarting/TaskStopped/TaskFailed/TimeoutHit/...)
//!     │     └─► publish(ActorExhausted | ActorDead)
//!     └─► subscriber_listener (in Supervisor)
//!           ├─► AliveTracker.update()
//!           └─► SubscriberSet.emit{,_arc}() ──► MySubscriber.on_event()
//! ```
//!
//! ## Run
//! Requires the `events` feature to export `Event`/`EventKind` types.
//! ```bash
//! cargo run --example custom_subscriber --features events
//! ```

use std::{sync::Arc, time::Duration};
use taskvisor::{
    BackoffPolicy, Config, Event, EventKind, RestartPolicy, Subscribe, Supervisor, TaskError,
    TaskFn, TaskRef, TaskSpec,
};
use tokio_util::sync::CancellationToken;

/// A simple console subscriber that prints selected events.
/// In real life, you could export metrics, ship logs, send alerts, etc.
struct ConsoleSubscriber;

#[async_trait::async_trait]
impl Subscribe for ConsoleSubscriber {
    async fn on_event(&self, ev: &Event) {
        match ev.kind {
            EventKind::TaskStarting => {
                if let (Some(task), Some(attempt)) = (ev.task.as_deref(), ev.attempt) {
                    println!("[sub] starting: task={task} attempt={attempt}");
                }
            }
            EventKind::TaskStopped => {
                if let Some(task) = ev.task.as_deref() {
                    println!("[sub] stopped:  task={task}");
                }
            }
            EventKind::TaskFailed => {
                let task = ev.task.as_deref().unwrap_or("<unknown>");
                let attempt = ev.attempt.unwrap_or_default();
                let err = ev.error.as_deref().unwrap_or("<no error>");
                println!("[sub] failed:   task={task} attempt={attempt} err={err}");
            }
            EventKind::TimeoutHit => {
                let task = ev.task.as_deref().unwrap_or("<unknown>");
                let dur = ev.timeout.map(|d| format!("{d:?}")).unwrap_or_default();
                println!("[sub] timeout:  task={task} timeout={dur}");
            }
            EventKind::BackoffScheduled => {
                let task = ev.task.as_deref().unwrap_or("<unknown>");
                let delay = ev.delay.map(|d| format!("{d:?}")).unwrap_or_default();
                let attempt = ev.attempt.unwrap_or_default();
                let why = ev.error.as_deref().unwrap_or("");
                println!("[sub] backoff:  task={task} delay={delay} after attempt={attempt} {why}");
            }
            EventKind::ActorExhausted => {
                if let Some(task) = ev.task.as_deref() {
                    println!("[sub] exhausted: task={task}");
                }
            }
            EventKind::ActorDead => {
                let task = ev.task.as_deref().unwrap_or("<unknown>");
                let err = ev.error.as_deref().unwrap_or("<no error>");
                println!("[sub] dead:     task={task} err={err}");
            }
            EventKind::TaskAdded => {
                if let Some(task) = ev.task.as_deref() {
                    println!("[sub] added:    task={task}");
                }
            }
            EventKind::TaskRemoved => {
                if let Some(task) = ev.task.as_deref() {
                    println!("[sub] removed:  task={task}");
                }
            }
            EventKind::ShutdownRequested => {
                println!("[sub] shutdown requested");
            }
            EventKind::AllStoppedWithin => {
                println!("[sub] all stopped within grace");
            }
            EventKind::GraceExceeded => {
                println!("[sub] grace exceeded");
            }
            // Noise we ignore in this demo:
            EventKind::SubscriberPanicked
            | EventKind::SubscriberOverflow
            | EventKind::TaskAddRequested
            | EventKind::TaskRemoveRequested => {}
        }
    }

    fn name(&self) -> &'static str {
        "console"
    }

    fn queue_capacity(&self) -> usize {
        1024
    }
}

/// One-shot task that prints and exits successfully.
fn oneshot_ok(name: &'static str) -> TaskSpec {
    // Use an owned String inside the future to satisfy 'static bounds cleanly.
    let n = name.to_owned();

    let task: TaskRef = TaskFn::arc(name, move |ctx: CancellationToken| {
        let n = n.clone();
        async move {
            if ctx.is_cancelled() {
                return Ok(());
            }
            println!("[{n}] doing one-shot work...");
            tokio::time::sleep(Duration::from_millis(300)).await;
            println!("[{n}] success");
            Ok::<(), TaskError>(())
        }
    });

    TaskSpec::new(
        task,
        RestartPolicy::Never,
        BackoffPolicy::default(),
        Some(Duration::from_secs(5)),
    )
}

/// One-shot task that fails on purpose (to show TaskFailed / ActorExhausted).
fn oneshot_fail(name: &'static str) -> TaskSpec {
    let n = name.to_owned();

    let task: TaskRef = TaskFn::arc(name, move |ctx: CancellationToken| {
        let n = n.clone();
        async move {
            if ctx.is_cancelled() {
                return Ok(());
            }
            println!("[{n}] starting and will fail...");
            tokio::time::sleep(Duration::from_millis(250)).await;
            Err(TaskError::Fail {
                error: "boom (demo failure)".to_string(),
            })
        }
    });

    // No restart, it'll fail once and exit; registry will clean it up.
    TaskSpec::new(
        task,
        RestartPolicy::Never,
        BackoffPolicy::default(),
        Some(Duration::from_secs(2)),
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    println!("🔌 custom_subscriber demo (run with --features events)\n");

    // Basic config; no global concurrency limit; no default timeout (we set per-task).
    let cfg = Config::default();

    // Register our custom subscriber.
    let subs: Vec<Arc<dyn Subscribe>> = vec![Arc::new(ConsoleSubscriber)];

    // Create supervisor with subscribers attached.
    let sup = Supervisor::new(cfg, subs);

    // Two one-shot tasks: one succeeds, one fails.
    let tasks = vec![oneshot_ok("alpha"), oneshot_fail("bravo")];

    // Run until all tasks complete (since both are one-shot, this will exit naturally).
    sup.run(tasks).await?;

    println!("\nfinished");
    Ok(())
}
