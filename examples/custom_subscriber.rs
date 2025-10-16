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
//!     │     ├─► publish(TaskStarting / TaskStopped / TaskFailed / TimeoutHit / ...)
//!     │     └─► publish(ActorExhausted | ActorDead)
//!     └─► subscriber_listener (in Supervisor)
//!           ├─► AliveTracker.update()
//!           └─► SubscriberSet.emit{,_arc}() ──► MySubscriber.on_event()
//! ```
//!
//! ## Run
//! Requires the `events` feature to export [`Event`] and [`EventKind`].
//! ```bash
//! cargo run --example custom_subscriber --features events
//! ```

use std::{sync::Arc, time::Duration};
use taskvisor::{
    BackoffPolicy, BackoffSource, Config, Event, EventKind, RestartPolicy, Subscribe, Supervisor,
    TaskError, TaskFn, TaskRef, TaskSpec,
};
use tokio_util::sync::CancellationToken;

/// A simple console subscriber that prints selected events.
/// In real life, you could export metrics, ship logs, or trigger alerts.
struct ConsoleSubscriber;

#[async_trait::async_trait]
impl Subscribe for ConsoleSubscriber {
    async fn on_event(&self, ev: &Event) {
        match ev.kind {
            // === Lifecycle ===
            EventKind::TaskStarting => {
                println!(
                    "[sub] starting: task={} attempt={}",
                    ev.task.as_deref().unwrap_or("<unknown>"),
                    ev.attempt.unwrap_or(0)
                );
            }
            EventKind::TaskStopped => {
                println!(
                    "[sub] stopped:  task={}",
                    ev.task.as_deref().unwrap_or("<unknown>")
                );
            }
            EventKind::TaskFailed => {
                println!(
                    "[sub] failed:   task={} attempt={} reason={}",
                    ev.task.as_deref().unwrap_or("<unknown>"),
                    ev.attempt.unwrap_or(0),
                    ev.reason.as_deref().unwrap_or("<none>")
                );
            }
            EventKind::TimeoutHit => {
                let dur = ev
                    .timeout_ms
                    .map(|v| format!("{}ms", v))
                    .unwrap_or_default();
                println!(
                    "[sub] timeout:  task={} timeout={}",
                    ev.task.as_deref().unwrap_or("<unknown>"),
                    dur
                );
            }
            EventKind::BackoffScheduled => {
                let delay = ev.delay_ms.map(|v| format!("{}ms", v)).unwrap_or_default();
                let src = match ev.backoff_source {
                    Some(BackoffSource::Success) => "success",
                    Some(BackoffSource::Failure) => "failure",
                    None => "unknown",
                };
                println!(
                    "[sub] backoff:  task={} delay={} after_attempt={} source={} reason={}",
                    ev.task.as_deref().unwrap_or("<unknown>"),
                    delay,
                    ev.attempt.unwrap_or(0),
                    src,
                    ev.reason.as_deref().unwrap_or("<none>")
                );
            }

            // === Actor terminal ===
            EventKind::ActorExhausted => {
                println!(
                    "[sub] exhausted: task={} reason={}",
                    ev.task.as_deref().unwrap_or("<unknown>"),
                    ev.reason.as_deref().unwrap_or("<policy>")
                );
            }
            EventKind::ActorDead => {
                println!(
                    "[sub] dead:     task={} reason={}",
                    ev.task.as_deref().unwrap_or("<unknown>"),
                    ev.reason.as_deref().unwrap_or("<fatal>")
                );
            }

            // === Management ===
            EventKind::TaskAdded => {
                println!(
                    "[sub] added:    task={}",
                    ev.task.as_deref().unwrap_or("<unknown>")
                );
            }
            EventKind::TaskRemoved => {
                println!(
                    "[sub] removed:  task={}",
                    ev.task.as_deref().unwrap_or("<unknown>")
                );
            }

            // === Shutdown ===
            EventKind::ShutdownRequested => {
                println!("[sub] shutdown requested");
            }
            EventKind::AllStoppedWithinGrace => {
                println!("[sub] all stopped within grace");
            }
            EventKind::GraceExceeded => {
                println!("[sub] grace exceeded");
            }

            // === Ignored ===
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

/// One-shot task that fails on purpose (to demonstrate TaskFailed / ActorExhausted).
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

    TaskSpec::new(
        task,
        RestartPolicy::Never,
        BackoffPolicy::default(),
        Some(Duration::from_secs(2)),
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    println!("custom_subscriber demo (run with --features events)\n");

    let cfg = Config::default();
    let subs: Vec<Arc<dyn Subscribe>> = vec![Arc::new(ConsoleSubscriber)];
    let sup = Supervisor::new(cfg, subs);

    let tasks = vec![oneshot_ok("alpha"), oneshot_fail("bravo")];
    sup.run(tasks).await?;

    println!("\nfinished");
    Ok(())
}
