use std::sync::Arc;

use tokio::{sync::Semaphore, task::JoinSet};
use tokio_util::sync::CancellationToken;

use crate::{
    actor::{TaskActor, TaskActorParams},
    alive::AliveTracker,
    bus::Bus,
    config::Config,
    error::RuntimeError,
    event::{Event, EventKind},
    observer::Observer,
    task_spec::TaskSpec,
};

pub struct Supervisor<O: Observer + Send + Sync + 'static> {
    pub cfg: Config,
    pub obs: Arc<O>,
    pub bus: Bus,
}

impl<Obs: Observer + Send + Sync + 'static> Supervisor<Obs> {
    pub fn new(cfg: Config, observer: Obs) -> Self {
        Self {
            bus: Bus::new(cfg.bus_capacity),
            obs: Arc::new(observer),
            cfg,
        }
    }

    pub async fn run(&self, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
        let semaphore = self.build_semaphore();
        let token = CancellationToken::new();
        self.observer_listener();

        let alive = AliveTracker::new();
        alive.spawn_listener(self.bus.subscribe());

        let mut set = JoinSet::new();
        self.task_actors(&mut set, &token, &semaphore, tasks);
        self.shutdown(&mut set, &token, &alive).await
    }

    fn observer_listener(&self) {
        let mut rx = self.bus.subscribe();
        let obs = self.obs.clone();

        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                obs.on_event(&ev).await;
            }
        });
    }

    fn build_semaphore(&self) -> Option<Arc<Semaphore>> {
        match self.cfg.max_concurrent {
            0 => None,
            n => Some(Arc::new(Semaphore::new(n))),
        }
    }

    fn task_actors(
        &self,
        set: &mut JoinSet<()>,
        runtime_token: &CancellationToken,
        global_sem: &Option<Arc<Semaphore>>,
        tasks: Vec<TaskSpec>,
    ) {
        for spec in tasks {
            let actor = TaskActor::new(
                spec.task.clone(),
                TaskActorParams {
                    restart: spec.restart,
                    backoff: spec.backoff,
                    attempt_timeout: spec.timeout,
                },
                self.bus.clone(),
                global_sem.clone(),
            );
            let child = runtime_token.child_token();
            set.spawn(actor.run(child));
        }
    }

    async fn shutdown(
        &self,
        set: &mut JoinSet<()>,
        runtime_token: &CancellationToken,
        alive: &AliveTracker,
    ) -> Result<(), RuntimeError> {
        let ctrlc = tokio::signal::ctrl_c();

        tokio::select! {
            _ = ctrlc => {
                self.bus.publish(Event::now(EventKind::ShutdownRequested));
                runtime_token.cancel();
                self.wait_all_with_grace(set, alive).await
            }
            _ = async { while set.join_next().await.is_some() {} } => {
                Ok(())
            }
        }
    }

    async fn wait_all_with_grace(
        &self,
        set: &mut JoinSet<()>,
        alive: &AliveTracker,
    ) -> Result<(), RuntimeError> {
        let grace = self.cfg.grace;
        let done = async { while set.join_next().await.is_some() {} };
        let timed = tokio::time::timeout(grace, done).await;

        match timed {
            Ok(_) => {
                self.bus.publish(Event::now(EventKind::AllStoppedWithin));
                Ok(())
            }
            Err(_) => {
                self.bus.publish(Event::now(EventKind::GraceExceeded));
                let stuck = alive.snapshot().await;
                Err(RuntimeError::GraceExceeded { grace, stuck })
            }
        }
    }
}
