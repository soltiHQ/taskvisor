use std::sync::Arc;
use std::time::Duration;

use tokio::{select, sync::Semaphore, time};
use tokio_util::sync::CancellationToken;

use crate::{
    bus::Bus,
    event::{Event, EventKind},
    policy::RestartPolicy,
    runner::run_once,
    strategy::BackoffStrategy,
    task::Task,
};

#[derive(Clone)]
pub struct TaskActorParams {
    pub restart: RestartPolicy,
    pub backoff: BackoffStrategy,
    pub attempt_timeout: Option<Duration>,
}

pub struct TaskActor {
    pub task: Arc<dyn Task>,
    pub params: TaskActorParams,
    pub bus: Bus,
    pub global_sem: Option<Arc<Semaphore>>,
}

impl TaskActor {
    pub fn new(
        task: Arc<dyn Task>,
        params: TaskActorParams,
        bus: Bus,
        global_sem: Option<Arc<Semaphore>>,
    ) -> Self {
        Self {
            task,
            params,
            bus,
            global_sem,
        }
    }

    pub async fn run(self, runtime_token: CancellationToken) {
        let mut attempt: u64 = 0;
        let mut prev_delay: Option<Duration> = None;

        loop {
            if runtime_token.is_cancelled() {
                break;
            }

            let _permit_guard = match &self.global_sem {
                Some(sem) => {
                    let permit_fut = sem.clone().acquire_owned();
                    tokio::pin!(permit_fut);
                    select! {
                        res = &mut permit_fut => {
                            match res {
                                Ok(permit) => Some(permit),
                                Err(_) => None,
                            }
                        }
                        _ = runtime_token.cancelled() => { break; }
                    }
                }
                None => None,
            };

            attempt += 1;
            self.bus.publish(
                Event::now(EventKind::TaskStarting)
                    .with_task(self.task.name())
                    .with_attempt(attempt),
            );

            let res = run_once(
                self.task.as_ref(),
                &runtime_token,
                self.params.attempt_timeout,
                &self.bus,
            )
            .await;

            match res {
                Ok(()) => {
                    prev_delay = None;
                    match self.params.restart {
                        RestartPolicy::Never => break,
                        RestartPolicy::OnFailure => break,
                        RestartPolicy::Always => continue,
                    }
                }
                Err(e) => {
                    let should_retry = matches!(
                        self.params.restart,
                        RestartPolicy::OnFailure | RestartPolicy::Always
                    );
                    if !should_retry {
                        break;
                    }

                    let delay = self.params.backoff.next(prev_delay);
                    prev_delay = Some(delay);

                    self.bus.publish(
                        Event::now(EventKind::BackoffScheduled)
                            .with_task(self.task.name())
                            .with_delay(delay)
                            .with_attempt(attempt)
                            .with_error(e.to_string()),
                    );

                    let sleep = time::sleep(delay);
                    tokio::pin!(sleep);
                    select! {
                        _ = &mut sleep => {}
                        _ = runtime_token.cancelled() => { break; }
                    }
                }
            }
        }
    }
}
