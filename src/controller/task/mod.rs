mod config;
mod worker;

pub use config::ControllerConfig;

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::{BackoffPolicy, JitterPolicy, RestartPolicy, TaskFn, TaskRef, TaskSpec};

// Re-export public entrypoints for external users (so examples import only from controller::task)
pub use crate::controller::runtime::bind_supervisor;
pub use crate::controller::inbox::submit;
pub use crate::controller::admission::Admission;
pub use crate::controller::spec::ControllerSpec;
use crate::controller::inbox;

/// Create controller task spec (engine runs inside).
pub fn controller(config: ControllerConfig) -> TaskSpec {
    let config = Arc::new(config);

    // Initialize inbox once (idempotent).
    inbox::init(config.capacity);

    let task: TaskRef = TaskFn::arc("core-controller", {
        let cfg = Arc::clone(&config);
        move |ctx: CancellationToken| {
            let cfg = Arc::clone(&cfg);
            async move { worker::run(ctx, cfg).await }
        }
    });

    TaskSpec::new(
        task,
        RestartPolicy::Always,
        BackoffPolicy {
            success_delay: Some(Duration::from_secs(1)),
            first: Duration::from_millis(100),
            max: Duration::from_millis(100),
            jitter: JitterPolicy::None,
            factor: 1.0,
        },
        None,
    )
}