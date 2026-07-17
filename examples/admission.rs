//! # Admission outcomes
//!
//! `submit` returning `Ok(id)` means that the controller accepted the request.
//! It does not mean that the slot admitted the task. The admission decision is asynchronous.
//!
//! `submit_and_watch` also returns a `TaskWaiter`:
//!
//! - an admitted task resolves to its final runtime outcome;
//! - a task that never starts resolves to `TaskOutcome::Rejected` with a typed `RejectionKind` and diagnostic reason.
//!
//! This example shows both paths and reads a live controller snapshot.
//! Use the waiter when rejection affects application logic.
//! Events are best-effort and are better suited to logs and metrics.
//!
//! Run with `cargo run --example admission`.

use std::sync::Arc;
use std::time::Duration;

use taskvisor::prelude::*;
use taskvisor::{ControllerConfig, ControllerSpec, RejectionKind};
use tokio::sync::Notify;

/// A job that runs for `duration`, observing cancellation.
fn job(name: &'static str, duration: Duration) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(name, move |ctx| async move {
        ctx.run_until_cancelled(tokio::time::sleep(duration))
            .await?;
        Ok(())
    });
    TaskSpec::once(task)
}

/// A job that reports when its body starts, then waits for an explicit release.
fn gated_job(name: &'static str, started: Arc<Notify>, release: Arc<Notify>) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(name, move |ctx| {
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        async move {
            started.notify_one();
            ctx.run_until_cancelled(release.notified()).await?;
            Ok(())
        }
    });
    TaskSpec::once(task)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_controller(ControllerConfig::default())
        .build();
    let handle = supervisor.serve();

    println!("Slot 'deploy' admits at most one task at a time.\n");

    // 1) The slot is idle: this submission is admitted and starts running.
    //    Every submission below uses .with_slot("deploy"): they contend for one slot.
    println!("1) submit deploy-v1 (Queue) to the idle slot");
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (_id, v1) = handle
        .submit_and_watch(
            ControllerSpec::queue(gated_job(
                "deploy-v1",
                Arc::clone(&started),
                Arc::clone(&release),
            ))
            .with_slot("deploy"),
        )
        .await?;
    // The task body, rather than a timer, confirms that registry admission completed.
    started.notified().await;
    println!("    deploy-v1 admitted, now running\n");

    // Pull the controller's live state directly: no parsing of bus events.
    if let Some(snap) = handle.controller_snapshot().await {
        let deploy = snap.slot("deploy");
        println!(
            "    controller: {} running, {} queued; deploy status={:?} depth={}\n",
            snap.running_count(),
            snap.total_queued(),
            deploy.map(|s| s.status),
            deploy.map_or(0, |s| s.queue_depth),
        );
    }

    // 2) While deploy-v1 holds the slot, a DropIfRunning submission is refused.
    //     submit() would only report rejection on the best-effort event path;
    //     submit_and_watch() gives this submission a dedicated outcome channel.
    println!("2) submit deploy-v2 (DropIfRunning) while the slot is busy");
    let (_id, v2) = handle
        .submit_and_watch(
            ControllerSpec::drop_if_running(job("deploy-v2", Duration::from_millis(200)))
                .with_slot("deploy"),
        )
        .await?;
    match v2.wait().await? {
        TaskOutcome::Rejected {
            kind: RejectionKind::SlotBusy,
            reason,
            ..
        } => {
            println!("    deploy-v2 -> Rejected ({reason}) - never ran\n");
        }
        other => println!("    deploy-v2 -> {other:?} (unexpected)\n"),
    }

    // 3) The admitted task still finishes normally.
    println!("3) await the admitted task");
    release.notify_one();
    println!("    deploy-v1 -> {:?}", v1.wait().await?);

    handle.shutdown().await?;
    Ok(())
}
