//! Lossless completion signals shared by registry clients and cleanup owners.

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::core::outcome::TaskOutcome;

/// Sender used to resolve a watched task with its final [`TaskOutcome`].
pub(crate) type OutcomeTx = oneshot::Sender<TaskOutcome>;

/// Shared terminal signal for callers waiting until registry cleanup is committed.
#[derive(Clone, Debug)]
pub(crate) struct RemovalCompletion {
    token: CancellationToken,
}

impl RemovalCompletion {
    pub(crate) fn new() -> Self {
        Self {
            token: CancellationToken::new(),
        }
    }

    pub(crate) async fn wait(&self) {
        self.token.cancelled().await;
    }

    pub(super) fn is_complete(&self) -> bool {
        self.token.is_cancelled()
    }

    pub(super) fn complete(&self) {
        self.token.cancel();
    }
}
