use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;

use crate::TaskError;

use super::task::ControllerConfig;
use super::spec::ControllerSpec;
use super::admission::Admission;
use super::{runtime, inbox};
use super::slots::Slot;

/// Controller main loop: consumes submissions and applies admission policies.
pub async fn run(ctx: CancellationToken, _cfg: Arc<ControllerConfig>) -> Result<(), TaskError> {
    if ctx.is_cancelled() {
        return Err(TaskError::Canceled);
    }

    // Runtime dependencies
    let sup = runtime::supervisor();
    let mut inbox = inbox::recv_guard().await;

    // Per-slot state
    let mut slots: HashMap<String, Slot> = HashMap::new();

    // Reconcile: keep queues moving even if we missed an event
    let mut tick = interval(Duration::from_millis(100));

    loop {
        tokio::select! {
            _ = ctx.cancelled() => break,

            // Reconcile periodically: if a slot was marked running but the task already died, start next.
            _ = tick.tick() => {
                for (name, slot) in slots.iter_mut() {
                    if slot.running && !sup.is_alive(name).await {
                        slot.running = false;
                        if let Some(next) = slot.queue.pop_front() {
                            let _ = sup.add_task(next);
                            slot.running = true;
                        }
                    }
                }
            }

            // New submission
            maybe = inbox.recv() => {
                let Some(req) = maybe else { break };

                // pull out fields without breaking privacy / lifetimes
                let name = req.name().to_string();
                let admission = req.admission.clone();
                let spec = req.spec;

                let entry = slots.entry(name.clone()).or_insert_with(Slot::new);

                // Refresh running flag if our view is stale
                if entry.running && !sup.is_alive(&name).await {
                    entry.running = false;
                }

                match (entry.running, admission) {
                    // Free slot: start immediately
                    (false, _) => {
                        let _ = sup.add_task(spec);
                        entry.running = true;
                    }

                    // Busy: Replace → enqueue to front and cancel current
                    (true, Admission::Replace) => {
                        entry.queue.push_front(spec);
                        let _ = sup.cancel(&name).await;

                        // If cancelled quickly, start next right away
                        if !sup.is_alive(&name).await {
                            entry.running = false;
                            if let Some(next) = entry.queue.pop_front() {
                                let _ = sup.add_task(next);
                                entry.running = true;
                            }
                        }
                    }

                    // Busy: Queue → enqueue to tail
                    (true, Admission::Queue) => {
                        entry.queue.push_back(spec);
                    }

                    // Busy: DropIfRunning → ignore
                    (true, Admission::DropIfRunning) => { /* no-op */ }
                }
            }
        }
    }

    Ok(())
}