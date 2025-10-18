use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::ControllerConfig;
use crate::TaskError;
use crate::controller::engine;

/// Controller worker entrypoint: delegates to the engine loop.
pub async fn run(ctx: CancellationToken, cfg: Arc<ControllerConfig>) -> Result<(), TaskError> {
    if ctx.is_cancelled() {
        return Err(TaskError::Canceled);
    }
    engine::run(ctx, cfg).await
}