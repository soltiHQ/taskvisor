//! # Latest-wins synchronization per tenant
//!
//! Use one controller slot per tenant when revisions for that tenant must not overlap.
//! Task names identify revisions; the shared slot identifies the tenant.
//!
//! ```text
//! tenant-42 slot: rev-1 running ──► rev-2 waits ──► rev-3 supersedes rev-2
//!                                                         │ rev-1 cleanup finishes
//!                                                         ▼
//!                                                    rev-3 starts
//!
//! tenant-17 slot: rev-1 runs independently ──► completes
//! ```
//!
//! `Replace` supersedes only the queue head. Existing FIFO items behind that head remain.
//! The `Notify` gates make the ordering deterministic for this demonstration;
//! production work normally waits on real I/O and observes cancellation through `TaskContext`.
//!
//! Expected outcomes: tenant-42 rev-1 is canceled, rev-2 is `SupersededByReplace`, and rev-3 completes.
//! Tenant-17 completes independently. Decisions use reliable waiters, not events.
//!
//! Run with `cargo run --example tenant_sync`.

use std::sync::Arc;

use taskvisor::prelude::*;
use taskvisor::{ControllerConfig, ControllerSpec, RejectionKind};
use tokio::sync::Notify;

#[derive(Debug)]
struct SyncGate {
    started: Notify,
    cancel_observed: Notify,
    finish: Notify,
    cancel_cleanup: Notify,
    hold_cancel_cleanup: bool,
}

impl SyncGate {
    fn new(hold_cancel_cleanup: bool) -> Arc<Self> {
        Arc::new(Self {
            started: Notify::new(),
            cancel_observed: Notify::new(),
            finish: Notify::new(),
            cancel_cleanup: Notify::new(),
            hold_cancel_cleanup,
        })
    }
}

fn tenant_sync(tenant: &'static str, revision: u64, gate: Arc<SyncGate>) -> TaskSpec {
    let name = format!("sync/{tenant}/rev-{revision}");
    let task: TaskRef = TaskFn::arc(move |ctx| {
        let gate = Arc::clone(&gate);
        async move {
            println!("  {tenant} revision {revision}: started");
            gate.started.notify_one();

            tokio::select! {
                biased;
                _ = ctx.cancelled() => {
                    println!("  {tenant} revision {revision}: replacement requested");
                    gate.cancel_observed.notify_one();
                    if gate.hold_cancel_cleanup {
                        gate.cancel_cleanup.notified().await;
                    }
                    println!("  {tenant} revision {revision}: cleanup finished");
                    Err(TaskError::Canceled)
                }
                _ = gate.finish.notified() => {
                    println!("  {tenant} revision {revision}: applied");
                    Ok(())
                }
            }
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

    let tenant_42_old = SyncGate::new(true);
    let tenant_42_latest = SyncGate::new(false);
    let tenant_17 = SyncGate::new(false);

    let old_waiter = handle
        .submit(
            ControllerSpec::replace(tenant_sync("tenant-42", 1, Arc::clone(&tenant_42_old)))
                .with_slot("tenant-42"),
        )
        .watch()
        .execute()
        .await?;
    tenant_42_old.started.notified().await;

    let other_waiter = handle
        .submit(
            ControllerSpec::replace(tenant_sync("tenant-17", 1, Arc::clone(&tenant_17)))
                .with_slot("tenant-17"),
        )
        .watch()
        .execute()
        .await?;
    tenant_17.started.notified().await;
    println!("different tenant slots are running together\n");

    let pending_waiter = handle
        .submit(
            ControllerSpec::replace(tenant_sync("tenant-42", 2, SyncGate::new(false)))
                .with_slot("tenant-42"),
        )
        .watch()
        .execute()
        .await?;
    tenant_42_old.cancel_observed.notified().await;
    println!("tenant-42 revision 2 waits for revision 1 cleanup\n");

    let latest_waiter = handle
        .submit(
            ControllerSpec::replace(tenant_sync("tenant-42", 3, Arc::clone(&tenant_42_latest)))
                .with_slot("tenant-42"),
        )
        .watch()
        .execute()
        .await?;
    match pending_waiter.wait().await? {
        TaskOutcome::Rejected {
            kind: RejectionKind::SupersededByReplace,
            ..
        } => println!("tenant-42 revision 2 -> superseded before start\n"),
        other => panic!("unexpected tenant-42 revision 2 outcome: {other:?}"),
    }

    tenant_42_old.cancel_cleanup.notify_one();
    println!("tenant-42 revision 1 -> {:?}", old_waiter.wait().await?);
    tenant_42_latest.started.notified().await;

    tenant_42_latest.finish.notify_one();
    tenant_17.finish.notify_one();
    let (latest_outcome, other_outcome) =
        tokio::try_join!(latest_waiter.wait(), other_waiter.wait())?;
    println!("tenant-42 revision 3 -> {latest_outcome:?}");
    println!("tenant-17 revision 1 -> {other_outcome:?}");

    handle.shutdown().await?;
    Ok(())
}
