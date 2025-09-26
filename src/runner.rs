use std::time::Duration;
use tokio::time;
use tokio_util::sync::CancellationToken;

use crate::{
    bus::Bus,
    error::TaskError,
    event::{Event, EventKind},
    task::Task,
};

// wrap panic as fatal

pub async fn run_once<T: Task + ?Sized>(
    task: &T,
    parent: &CancellationToken,
    attempt_timeout: Option<Duration>,
    bus: &Bus,
) -> Result<(), TaskError> {
    let child = parent.child_token();

    match attempt_timeout {
        Some(dur) if dur > Duration::ZERO => {
            let fut = task.run(child.clone());
            match time::timeout(dur, fut).await {
                Ok(res) => match res {
                    Ok(()) => {
                        bus.publish(Event::now(EventKind::TaskStopped).with_task(task.name()));
                        Ok(())
                    }
                    Err(e) => {
                        bus.publish(
                            Event::now(EventKind::TaskFailed)
                                .with_task(task.name())
                                .with_error(e.to_string()),
                        );
                        Err(e)
                    }
                },
                Err(_elapsed) => {
                    child.cancel();
                    bus.publish(
                        Event::now(EventKind::TimeoutHit)
                            .with_task(task.name())
                            .with_timeout(dur),
                    );
                    Err(TaskError::Timeout { timeout: dur })
                }
            }
        }
        _ => match task.run(child.clone()).await {
            Ok(()) => {
                bus.publish(Event::now(EventKind::TaskStopped).with_task(task.name()));
                Ok(())
            }
            Err(e) => {
                bus.publish(
                    Event::now(EventKind::TaskFailed)
                        .with_task(task.name())
                        .with_error(e.to_string()),
                );
                Err(e)
            }
        },
    }
}
