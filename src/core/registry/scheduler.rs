//! Bounded ingress and cooperative polling for registered task actors.

use std::{
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::Mutex,
    task::{Context, Poll},
};

use futures_util::{
    FutureExt, StreamExt,
    future::{AbortHandle, Abortable},
    stream::FuturesUnordered,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{core::actor::ActorExitReason, identity::TaskId};

type ActorResult = Result<ActorExitReason, ActorJoinError>;
type ScheduledFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// One producer (the serialized registry listener) only needs one buffered handoff.
const SCHEDULER_QUEUE_CAPACITY: usize = 1;

/// Failure reported by the scheduler instead of Tokio's per-task `JoinError`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ActorJoinError {
    Panicked,
    Aborted,
}

impl ActorJoinError {
    pub(super) const fn is_panic(self) -> bool {
        matches!(self, Self::Panicked)
    }

    #[cfg(test)]
    pub(super) const fn is_cancelled(self) -> bool {
        matches!(self, Self::Aborted)
    }
}

/// Registry-owned control and result handle for one scheduled actor future.
pub(super) struct ActorHandle {
    abort: AbortHandle,
    result: oneshot::Receiver<ActorResult>,
}

impl ActorHandle {
    pub(super) fn abort(&self) {
        self.abort.abort();
    }
}

impl Future for ActorHandle {
    type Output = ActorResult;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.result).poll(cx) {
            Poll::Ready(Ok(result)) => Poll::Ready(result),
            Poll::Ready(Err(_closed)) => Poll::Ready(Err(ActorJoinError::Aborted)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Guarantees one result and one reliable completion identity even when a queued future is dropped.
struct CompletionGuard {
    id: TaskId,
    result: Option<oneshot::Sender<ActorResult>>,
    completion_tx: mpsc::UnboundedSender<TaskId>,
}

impl CompletionGuard {
    fn complete(mut self, result: ActorResult) {
        if let Some(tx) = self.result.take() {
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let _ = tx.send(result);
            }));
        }
        let _ = self.completion_tx.send(self.id);
    }
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.result.take() {
            let _ = tx.send(Err(ActorJoinError::Aborted));
            let _ = self.completion_tx.send(self.id);
        }
    }
}

/// One actor future waiting in, or already polled by, the central scheduler.
pub(super) struct ScheduledActor {
    future: ScheduledFuture,
}

enum SchedulerCommand {
    One(ScheduledActor),
    Batch(Vec<ScheduledActor>),
}

impl ScheduledActor {
    pub(super) fn new(
        id: TaskId,
        completion_tx: mpsc::UnboundedSender<TaskId>,
        future: impl Future<Output = ActorExitReason> + Send + 'static,
    ) -> (Self, ActorHandle) {
        let (abort, registration) = AbortHandle::new_pair();
        let (result_tx, result_rx) = oneshot::channel();
        let guard = CompletionGuard {
            id,
            result: Some(result_tx),
            completion_tx,
        };
        let future = async move {
            let result = AssertUnwindSafe(Abortable::new(future, registration))
                .catch_unwind()
                .await;
            let result = match result {
                Ok(Ok(reason)) => Ok(reason),
                Ok(Err(_aborted)) => Err(ActorJoinError::Aborted),
                Err(_panic) => Err(ActorJoinError::Panicked),
            };
            guard.complete(result);
        }
        .boxed();

        (
            Self { future },
            ActorHandle {
                abort,
                result: result_rx,
            },
        )
    }
}

/// One Tokio task polling all registered actor futures.
///
/// The command channel is bounded. The active set stores futures as data, so registered tasks do not each allocate a Tokio task while waiting for an attempt permit or retry delay.
pub(super) struct ActorScheduler {
    tx: mpsc::Sender<SchedulerCommand>,
    rx: Mutex<Option<mpsc::Receiver<SchedulerCommand>>>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl ActorScheduler {
    pub(super) fn new() -> Self {
        let (tx, rx) = mpsc::channel(SCHEDULER_QUEUE_CAPACITY);
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
            handle: Mutex::new(None),
        }
    }

    pub(super) fn spawn(&self, runtime_token: CancellationToken) {
        let mut rx = self
            .rx
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .expect("actor scheduler starts exactly once");
        let handle = tokio::spawn(async move {
            let mut active = FuturesUnordered::<ScheduledFuture>::new();
            let mut closing = false;
            loop {
                if closing && active.is_empty() {
                    break;
                }
                tokio::select! {
                    _ = runtime_token.cancelled(), if !closing => {
                        closing = true;
                        rx.close();
                        while let Ok(command) = rx.try_recv() {
                            Self::push_command(&mut active, command);
                        }
                    }
                    command = rx.recv(), if !closing => match command {
                        Some(command) => Self::push_command(&mut active, command),
                        None if active.is_empty() => break,
                        None => closing = true,
                    },
                    completed = active.next(), if !active.is_empty() => {
                        if completed.is_none() && closing {
                            break;
                        }
                    }
                }
            }
        });
        *self
            .handle
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(handle);
    }

    pub(super) async fn schedule(&self, actor: ScheduledActor) {
        // A closed scheduler drops `actor`; its completion guard resolves the registry handle and emits the reliable completion identity.
        let _ = self.tx.send(SchedulerCommand::One(actor)).await;
    }

    pub(super) async fn schedule_batch(&self, actors: Vec<ScheduledActor>) {
        if actors.is_empty() {
            return;
        }
        // One batch occupies one bounded handoff slot, matching atomic registry batch admission.
        let _ = self.tx.send(SchedulerCommand::Batch(actors)).await;
    }

    fn push_command(active: &mut FuturesUnordered<ScheduledFuture>, command: SchedulerCommand) {
        match command {
            SchedulerCommand::One(actor) => active.push(actor.future),
            SchedulerCommand::Batch(actors) => {
                active.extend(actors.into_iter().map(|actor| actor.future));
            }
        }
    }

    pub(super) async fn join(&self) -> bool {
        let handle = self
            .handle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let Some(handle) = handle else {
            return true;
        };
        handle.await.is_ok()
    }
}
