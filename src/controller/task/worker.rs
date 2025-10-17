use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::ControllerConfig;
use crate::TaskError;

pub async fn run(ctx: CancellationToken, cfg: Arc<ControllerConfig>) -> Result<(), TaskError> {
    if ctx.is_cancelled() {
        return Err(TaskError::Canceled);
    }
    println!("task started");
    Ok(())
}
