//! Starts accepted actors and polls physical reaper work.
//!
//! The registry listener starts [`ActorRuntime`] during supervisor startup. Admission gives it actors
//! only after registry commit. Force-abort work enters the shared [`AttemptReaper`] and is polled by one coordinator task.
//!
//! Closing the coordinator stops new reaper admission and drains queued work.
//! The shutdown join waits for the coordinator only when no physical attempt is active.
//! A blocked force-aborted actor therefore cannot extend registry shutdown.

use std::sync::Mutex;

use futures_util::{StreamExt, stream::FuturesUnordered};
use tokio::{sync::mpsc, task::JoinHandle};

use super::{
    actor::ScheduledActor,
    reaper::{AttemptReaper, ReapFuture, ReaperCommand},
};

/// Spawns accepted actors and owns the physical reaper coordinator.
pub(in crate::core::registry) struct ActorRuntime {
    /// Shared force-abort owner used by registry entries.
    attempt_reaper: AttemptReaper,
    /// Coordinator receiver consumed on first spawn.
    reaper_rx: Mutex<Option<mpsc::UnboundedReceiver<ReaperCommand>>>,
    /// Physical coordinator task while it can be joined.
    reaper_handle: Mutex<Option<JoinHandle<()>>>,
}

impl ActorRuntime {
    /// Creates an actor runtime with an idle reaper coordinator.
    pub(in crate::core::registry) fn new() -> Self {
        let (reaper_tx, reaper_rx) = mpsc::unbounded_channel();
        Self {
            attempt_reaper: AttemptReaper::new(reaper_tx),
            reaper_rx: Mutex::new(Some(reaper_rx)),
            reaper_handle: Mutex::new(None),
        }
    }

    /// Returns a shared owner for force-aborted attempts.
    pub(in crate::core::registry) fn attempt_reaper(&self) -> AttemptReaper {
        self.attempt_reaper.clone()
    }

    /// Returns attempts that still retain physical ownership.
    pub(in crate::core::registry) fn reaping_attempts(&self) -> usize {
        self.attempt_reaper.active()
    }

    /// Starts the physical reaper coordinator.
    ///
    /// # Panics
    ///
    /// Panics when called more than once or outside a Tokio runtime.
    pub(in crate::core::registry) fn spawn(&self) {
        let mut reaper_rx = self
            .reaper_rx
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .expect("attempt reaper starts exactly once");
        let handle = tokio::spawn(async move {
            let mut active = FuturesUnordered::<ReapFuture>::new();
            let mut closing = false;
            loop {
                if closing && active.is_empty() {
                    break;
                }
                tokio::select! {
                    command = reaper_rx.recv(), if !closing => match command {
                        Some(ReaperCommand::Reap(future)) => active.push(future),
                        Some(ReaperCommand::Close) | None => {
                            closing = true;
                            reaper_rx.close();
                            while let Ok(command) = reaper_rx.try_recv() {
                                if let ReaperCommand::Reap(future) = command {
                                    active.push(future);
                                }
                            }
                        }
                    },
                    completed = active.next(), if !active.is_empty() => {
                        debug_assert!(completed.is_some());
                    }
                }
            }
        });
        *self
            .reaper_handle
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(handle);
    }

    /// Spawns one actor after registry commit.
    pub(in crate::core::registry) fn schedule(&self, actor: ScheduledActor) {
        actor.spawn();
    }

    /// Spawns an accepted actor batch in iterator order.
    pub(in crate::core::registry) fn schedule_batch(
        &self,
        actors: impl IntoIterator<Item = ScheduledActor>,
    ) {
        for actor in actors {
            actor.spawn();
        }
    }

    /// Closes the coordinator and performs a best-effort join.
    ///
    /// Active reaper attempts skip the join and leave the coordinator handle retained by this runtime.
    /// The method returns `false` only when an idle coordinator task fails while being joined.
    pub(in crate::core::registry) async fn join(&self) -> bool {
        self.attempt_reaper.close();
        if self.attempt_reaper.active() != 0 {
            return true;
        }
        let handle = self
            .reaper_handle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        match handle {
            Some(handle) => handle.await.is_ok(),
            None => true,
        }
    }
}
