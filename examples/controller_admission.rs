//! # Watched controller admission
//!
//! Use this pattern when application logic must know whether submitted work ran or was rejected.
//!
//! ```text
//! prepare_submission ──► known TaskId; no command sent yet
//!          │ submit + watch + execute
//!          ▼
//! controller intake ──► slot decision
//!                           ├── admitted ──► registry ──► task ──► final TaskOutcome
//!                           └── rejected ──► TaskOutcome::Rejected; task never starts
//! ```
//!
//! The returned `TaskWaiter` confirms command intake, not positive admission.
//! The waiter resolves to the admitted task's final outcome or a typed rejection.
//!
//! This example produces `TaskOutcome::Completed` for `deploy-v1`.
//! For `deploy-v2`, it produces `TaskOutcome::Rejected` with `RejectionKind::SlotBusy`.
//!
//! `controller_snapshot` is a rolling, non-atomic diagnostics view.
//! Do not use it for decisions that require a reliable result.
//! Use the waiter instead; events remain best-effort.
//!
//! Run with `cargo run --example controller_admission`.

use std::sync::Arc;
use std::time::Duration;

use taskvisor::prelude::*;
use taskvisor::{ControllerConfig, ControllerSpec, RejectionKind};
use tokio::sync::Notify;

/// A job that runs for `duration`, observing cancellation.
fn job(name: &'static str, duration: Duration) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(move |ctx| async move {
        ctx.run_until_cancelled(tokio::time::sleep(duration))
            .await?;
        Ok(())
    });
    TaskSpec::once(name, task)
}

/// A job that reports when its body starts, then waits for an explicit release.
fn gated_job(name: &'static str, started: Arc<Notify>, release: Arc<Notify>) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(move |ctx| {
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        async move {
            started.notify_one();
            ctx.run_until_cancelled(release.notified()).await?;
            Ok(())
        }
    });
    TaskSpec::once(name, task)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_controller(ControllerConfig::default())
        .build();
    let handle = supervisor.serve()?;

    println!("Slot 'deploy' admits at most one task at a time.\n");

    // 1) The slot is idle: this submission is admitted and starts running.
    //    Every submission below uses .with_slot("deploy"): they contend for one slot.
    println!("1) submit deploy-v1 (Queue) to the idle slot");
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let prepared = handle.prepare_submission(
        ControllerSpec::queue(gated_job(
            "deploy-v1",
            Arc::clone(&started),
            Arc::clone(&release),
        ))
        .with_slot("deploy"),
    )?;
    println!("    reserved task id {} before intake", prepared.id());
    let v1 = prepared.submit().watch().execute().await?;
    started.notified().await;
    println!("    deploy-v1 admitted, now running\n");

    // Read the rolling controller diagnostics directly: no parsing of bus events.
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
    //     An unwatched execute reports only command intake; a later rejection then appears only
    //     on the best-effort event path. watch().execute() adds a dedicated outcome channel.
    println!("2) submit deploy-v2 (DropIfRunning) while the slot is busy");
    let v2 = handle
        .submit(
            ControllerSpec::drop_if_running(job("deploy-v2", Duration::from_millis(200)))
                .with_slot("deploy"),
        )
        .watch()
        .execute()
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
