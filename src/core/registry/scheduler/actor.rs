//! Splits one actor into a registry-owned handle and a task started after admission.
//!
//! [`ScheduledActor`] owns the future before spawn. [`ActorHandle`] is inserted into registry state
//! first and later owns the spawned Tokio task. A shared slot connects the pair because admission
//! must commit the handle before the actor can start.
//!
//! The actor wrapper catches outer panics and sends its result through a one-shot before it sends
//! the completion identity to the registry listener. A normal join resolves that result.
//! A forced abort moves the Tokio handle, result receiver, activity reservation, and physical latch to the attempt reaper.

use std::{
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};

use futures_util::FutureExt;
use tokio::{
    sync::{mpsc, oneshot},
    task::{JoinError, JoinHandle},
};

use crate::{
    core::{
        actor::ActorExitReason, registry::completion::RemovalCompletion,
        runner::dispose_panic_payload,
    },
    identity::TaskId,
};

use super::reaper::{AttemptReaper, AttemptReservation};

/// Result produced by a physical actor wrapper.
pub(super) type ActorResult = Result<ActorExitReason, ActorJoinError>;

/// User actor future retained until registry commit.
type ScheduledFuture = Pin<Box<dyn Future<Output = ActorExitReason> + Send + 'static>>;

/// Failure reported by an actor wrapper.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::core::registry) enum ActorJoinError {
    /// The actor panicked while it was polled.
    Panicked {
        /// Whether panic payload cleanup also failed.
        cleanup_poisoned: bool,
    },
    /// Logical join result after a force-abort transfer.
    Aborted,
}

impl ActorJoinError {
    /// Returns whether the actor panicked.
    pub(in crate::core::registry) const fn is_panic(self) -> bool {
        matches!(self, Self::Panicked { .. })
    }

    /// Returns whether panic payload cleanup also failed.
    pub(in crate::core::registry) const fn cleanup_poisoned(self) -> bool {
        matches!(
            self,
            Self::Panicked {
                cleanup_poisoned: true
            }
        )
    }

    /// Returns whether the actor was force-aborted.
    #[cfg(test)]
    pub(in crate::core::registry) const fn is_cancelled(self) -> bool {
        matches!(self, Self::Aborted)
    }
}

/// Registry-owned physical actor handle.
pub(in crate::core::registry) struct ActorHandle {
    /// Physical Tokio task after it is loaded from the shared spawn slot.
    join: Option<JoinHandle<Option<ActorResult>>>,
    /// Shared slot filled when the scheduled actor is spawned.
    join_slot: Arc<Mutex<Option<JoinHandle<Option<ActorResult>>>>>,
    /// Reliable actor result receiver.
    result: Option<oneshot::Receiver<ActorResult>>,
    /// Result received before the physical join completes.
    ready: Option<ActorResult>,
    /// Logical result produced without waiting for a physical join.
    logical: Option<ActorResult>,
    /// Metadata transferred to the reaper before force-abort.
    reservation: Option<AttemptReservation>,
    /// Owner for actors that outlive logical removal.
    reaper: AttemptReaper,
    /// Shared panic cleanup status for this actor.
    cleanup_poisoned: Arc<AtomicBool>,
}

impl ActorHandle {
    /// Takes the physical handle from the shared spawn slot when available.
    fn load_join(&mut self) {
        if self.join.is_none() {
            self.join = self
                .join_slot
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
        }
    }

    /// Transfers physical ownership to the reaper and reports logical abort.
    pub(in crate::core::registry) fn abort(&mut self) {
        self.load_join();
        let Some(join) = self.join.take() else {
            self.logical = Some(Err(ActorJoinError::Aborted));
            return;
        };
        let Some(reservation) = self.reservation.take() else {
            join.abort();
            self.logical = Some(Err(ActorJoinError::Aborted));
            return;
        };
        self.reaper
            .abort_actor(join, self.result.take(), self.ready.take(), reservation);
        self.logical = Some(Err(ActorJoinError::Aborted));
    }

    /// Tries to receive the reliable actor result without waiting.
    ///
    /// Genuine completion identifiers are sent only after this channel is ready.
    pub(in crate::core::registry) fn result_ready(&mut self) -> bool {
        if self.ready.is_none()
            && let Some(receiver) = self.result.as_mut()
        {
            match receiver.try_recv() {
                Ok(result) => {
                    self.ready = Some(result);
                    self.result = None;
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    self.result = None;
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
            }
        }
        self.ready.is_some()
    }

    /// Resolves a physical join and preserves a result sent just before join.
    ///
    /// The wrapper sends its result before it completes.
    /// A final receiver probe closes the race with the probe at the start of [`Future::poll`].
    fn complete_join(&mut self, joined: Result<Option<ActorResult>, JoinError>) -> ActorResult {
        self.join = None;
        self.reservation = None;
        let fallback = match joined {
            Ok(result) => result,
            Err(error) if error.is_panic() => {
                dispose_panic_payload(error.into_panic(), self.cleanup_poisoned.as_ref());
                Some(Err(ActorJoinError::Panicked {
                    cleanup_poisoned: self.cleanup_poisoned.load(Ordering::Acquire),
                }))
            }
            Err(_cancelled) => Some(Err(ActorJoinError::Aborted)),
        };

        let _ = self.result_ready();
        self.ready
            .take()
            .or(fallback)
            .unwrap_or(Err(ActorJoinError::Aborted))
    }
}

impl Future for ActorHandle {
    type Output = ActorResult;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(result) = this.logical.take() {
            return Poll::Ready(result);
        }
        this.load_join();

        if this.ready.is_none()
            && let Some(receiver) = this.result.as_mut()
        {
            match Pin::new(receiver).poll(cx) {
                Poll::Ready(Ok(result)) => {
                    this.ready = Some(result);
                    this.result = None;
                }
                Poll::Ready(Err(_closed)) => {
                    this.result = None;
                }
                Poll::Pending => {}
            }
        }

        let Some(join) = this.join.as_mut() else {
            return Poll::Pending;
        };
        let joined = match Pin::new(join).poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(joined) => joined,
        };
        Poll::Ready(this.complete_join(joined))
    }
}

impl Drop for ActorHandle {
    fn drop(&mut self) {
        self.load_join();
        if self.join.is_some() {
            self.abort();
        }
    }
}

/// One accepted actor waiting to be spawned after registry commit.
pub(in crate::core::registry) struct ScheduledActor {
    /// Stable task identity used by completion signals.
    id: TaskId,
    /// User actor future retained before spawn.
    future: ScheduledFuture,
    /// Reliable result channel owned by the actor wrapper.
    result: oneshot::Sender<ActorResult>,
    /// Completion signal channel for the registry listener.
    completion_tx: mpsc::UnboundedSender<TaskId>,
    /// Shared slot that transfers the spawned Tokio handle.
    join_slot: Arc<Mutex<Option<JoinHandle<Option<ActorResult>>>>>,
    /// Shared panic cleanup status for this actor.
    cleanup_poisoned: Arc<AtomicBool>,
}

/// Registry-owned identity, latches, and physical ownership for one actor task.
pub(in crate::core::registry) struct ActorRegistration {
    /// Stable task identity.
    pub(in crate::core::registry) id: TaskId,
    /// Unique name reserved through physical exit and terminal matching.
    pub(in crate::core::registry) name: Arc<str>,
    /// Current actor activity state.
    pub(in crate::core::registry) activity: Arc<AtomicBool>,
    /// Shared panic cleanup status.
    pub(in crate::core::registry) cleanup_poisoned: Arc<AtomicBool>,
    /// Latch completed after actor output is committed to deferred cleanup.
    pub(in crate::core::registry) physical_release: RemovalCompletion,
    /// Owner for force-aborted actor attempts.
    pub(in crate::core::registry) reaper: AttemptReaper,
    /// Completion signal channel for the registry listener.
    pub(in crate::core::registry) completion_tx: mpsc::UnboundedSender<TaskId>,
}

impl ScheduledActor {
    /// Prepares an actor and its registry-owned handle before commit.
    pub(in crate::core::registry) fn new(
        registration: ActorRegistration,
        future: impl Future<Output = ActorExitReason> + Send + 'static,
    ) -> (Self, ActorHandle) {
        let ActorRegistration {
            id,
            name,
            activity,
            cleanup_poisoned,
            physical_release,
            reaper,
            completion_tx,
        } = registration;
        let (result_tx, result_rx) = oneshot::channel();
        let join_slot = Arc::new(Mutex::new(None));
        let handle = ActorHandle {
            join: None,
            join_slot: Arc::clone(&join_slot),
            result: Some(result_rx),
            ready: None,
            logical: None,
            reservation: Some(AttemptReservation::new(
                id,
                name,
                activity,
                Arc::clone(&cleanup_poisoned),
                physical_release,
            )),
            reaper,
            cleanup_poisoned: Arc::clone(&cleanup_poisoned),
        };
        (
            Self {
                id,
                future: Box::pin(future),
                result: result_tx,
                completion_tx,
                join_slot,
                cleanup_poisoned,
            },
            handle,
        )
    }

    /// Spawns the actor and sends its result before its completion identity.
    pub(super) fn spawn(self) {
        let Self {
            id,
            future,
            result: result_tx,
            completion_tx,
            join_slot,
            cleanup_poisoned,
        } = self;
        let join = tokio::spawn(async move {
            let result = match AssertUnwindSafe(future).catch_unwind().await {
                Ok(reason) => Ok(reason),
                Err(payload) => {
                    dispose_panic_payload(payload, cleanup_poisoned.as_ref());
                    Err(ActorJoinError::Panicked {
                        cleanup_poisoned: cleanup_poisoned.load(Ordering::Acquire),
                    })
                }
            };
            let undelivered = result_tx.send(result).err();
            let _ = completion_tx.send(id);
            undelivered
        });
        *join_slot.lock().unwrap_or_else(|error| error.into_inner()) = Some(join);
    }
}

#[cfg(test)]
mod tests {
    use super::super::runtime::ActorRuntime;
    use super::*;

    #[test]
    fn joined_wrapper_rechecks_result_after_an_initial_empty_probe() {
        let runtime = ActorRuntime::new();
        let cleanup_poisoned = Arc::new(AtomicBool::new(false));
        let (result_tx, result_rx) = oneshot::channel();
        let mut handle = ActorHandle {
            join: None,
            join_slot: Arc::new(Mutex::new(None)),
            result: Some(result_rx),
            ready: None,
            logical: None,
            reservation: None,
            reaper: runtime.attempt_reaper(),
            cleanup_poisoned,
        };

        assert!(
            !handle.result_ready(),
            "the regression requires the first receiver probe to be empty"
        );
        result_tx
            .send(Ok(ActorExitReason::Completed))
            .expect("the actor result receiver remains owned by the handle");

        let result = handle.complete_join(Ok(None));
        assert!(
            matches!(result, Ok(ActorExitReason::Completed)),
            "the queued actor result must win over the missing join fallback"
        );
        assert!(handle.result.is_none());
    }
}
