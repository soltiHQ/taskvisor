use dashmap::DashMap;
use std::sync::{Arc, Weak};
use std::time::Instant;

use tokio::sync::{Mutex, RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{
    Supervisor,
    events::{Bus, Event, EventKind},
};

use super::{
    admission::Admission,
    config::ControllerConfig,
    error::SubmitError,
    slot::{SlotState, SlotStatus},
    spec::ControllerSpec,
};

/// Handle for submitting tasks to the controller.
#[derive(Clone)]
pub struct ControllerHandle {
    tx: mpsc::Sender<ControllerSpec>,
}

impl ControllerHandle {
    /// Submit a task (async, waits if queue is full).
    pub async fn submit(&self, spec: ControllerSpec) -> Result<(), SubmitError> {
        self.tx.send(spec).await.map_err(|_| SubmitError::Closed)
    }

    /// Try to submit without blocking (fails if queue full).
    pub fn try_submit(&self, spec: ControllerSpec) -> Result<(), SubmitError> {
        self.tx.try_send(spec).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => SubmitError::Full,
            mpsc::error::TrySendError::Closed(_) => SubmitError::Closed,
        })
    }
}

/// Controller manages task slots with admission policies.
///
/// Each slot can run at most one task at a time. New submissions are handled
/// according to the configured admission policy.
pub struct Controller {
    config: ControllerConfig,
    supervisor: Weak<Supervisor>,
    bus: Bus,

    // Concurrent slots map.
    slots: DashMap<String, Arc<Mutex<SlotState>>>,

    // Submission queue.
    tx: mpsc::Sender<ControllerSpec>,
    rx: RwLock<Option<mpsc::Receiver<ControllerSpec>>>,
}

impl Controller {
    /// Creates a new controller (must call .run() to start).
    pub fn new(config: ControllerConfig, supervisor: &Arc<Supervisor>, bus: Bus) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(config.queue_capacity);

        Arc::new(Self {
            config,
            supervisor: Arc::downgrade(supervisor),
            bus,
            slots: DashMap::new(),
            tx,
            rx: RwLock::new(Some(rx)),
        })
    }

    /// Returns a handle for submitting tasks.
    pub fn handle(&self) -> ControllerHandle {
        ControllerHandle {
            tx: self.tx.clone(),
        }
    }

    /// Starts the controller loop (spawns in background).
    pub fn run(self: Arc<Self>, token: CancellationToken) {
        tokio::spawn(async move {
            if let Err(e) = self.run_inner(token).await {
                eprintln!("[controller] error: {e:?}");
            }
        });
    }

    async fn run_inner(&self, token: CancellationToken) -> anyhow::Result<()> {
        let mut rx = self
            .rx
            .write()
            .await
            .take()
            .ok_or_else(|| anyhow::anyhow!("controller already running"))?;

        let mut bus_rx = self.bus.subscribe();

        loop {
            tokio::select! {
                _ = token.cancelled() => break,

                Some(spec) = rx.recv() => {
                    self.handle_submission(spec).await;
                }
                Ok(event) = bus_rx.recv() => {
                    self.handle_event(event).await;
                }
            }
        }

        Ok(())
    }

    /// Handles a new task submission.
    ///
    // Replace semantics:
    // - Enqueue the new spec at the head (latest-wins).
    // - If slot was Running -> transition to Terminating and request remove once.
    // - If already Terminating -> do NOT call remove again; just update the head.
    // - The next task actually starts in `on_task_finished` upon terminal event.
    async fn handle_submission(&self, spec: ControllerSpec) {
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };

        let slot_name = spec.slot_name().to_string();
        let admission = spec.admission;
        let task_spec = spec.task_spec;

        let slot_arc = self.get_or_create_slot(&slot_name);
        let mut slot = slot_arc.lock().await;

        if slot.queue.len() >= self.config.slot_capacity {
            eprintln!(
                "[controller] slot '{}' queue full ({}/{}), dropping submission",
                slot_name, slot.queue.len(), self.config.slot_capacity
            );
            return;
        }

        match (&slot.status, admission) {
            (SlotStatus::Idle, _) => {
                if let Err(e) = sup.add_task(task_spec) {
                    eprintln!("[controller] failed to add task '{slot_name}': {e}");
                    return;
                }
                slot.status = SlotStatus::Running {
                    started_at: Instant::now(),
                };
            }
            (SlotStatus::Running { .. }, Admission::Replace) => {
                if let Some(head) = slot.queue.front_mut() {
                    *head = task_spec;
                } else {
                    slot.queue.push_front(task_spec);
                }

                slot.status = SlotStatus::Terminating { cancelled_at: Instant::now() };
                if let Err(e) = sup.remove_task(&slot_name) {
                    eprintln!("[controller] failed to cancel task '{slot_name}': {e}");
                }
            }
            (SlotStatus::Running { .. }, Admission::Queue) => {
                slot.queue.push_back(task_spec);
            }
            (SlotStatus::Terminating { .. }, Admission::Replace) => {
                if let Some(head) = slot.queue.front_mut() {
                    *head = task_spec;
                } else {
                    slot.queue.push_front(task_spec);
                }
            }
            (SlotStatus::Terminating { .. }, _) => {
                slot.queue.push_back(task_spec);
            }
            (SlotStatus::Running { .. }, Admission::DropIfRunning) => {}
        }
    }

    /// Handles bus events (TaskStopped, TaskRemoved).
    async fn handle_event(&self, event: Arc<Event>) {
        match event.kind {
            EventKind::ActorExhausted | EventKind::ActorDead | EventKind::TaskRemoved => {
                self.on_task_finished(&event).await;
            }
            _ => {}
        }
    }

    /// Handles a terminal event (`ActorExhausted`, `ActorDead`, or `TaskRemoved`).
    ///
    /// IMPORTANT: Each slot is keyed by task name.
    /// TODO: maybe add `slot_name` with task_name as default.
    async fn on_task_finished(&self, event: &Event) {
        let Some(task_name) = event.task.as_deref() else {
            return;
        };
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };
        let Some(slot_arc) = self.slots.get(task_name).map(|e| e.clone()) else {
            return;
        };
        let mut slot = slot_arc.lock().await;

        if matches!(slot.status, SlotStatus::Idle) {
            return;
        }
        slot.status = SlotStatus::Idle;

        if let Some(next_spec) = slot.queue.pop_front() {
            if let Err(e) = sup.add_task(next_spec) {
                eprintln!("[controller] failed to start next task '{task_name}': {e}");
                return;
            }
            slot.status = SlotStatus::Running {
                started_at: Instant::now(),
            };
        }
        if matches!(slot.status, SlotStatus::Idle) && slot.queue.is_empty() {
            drop(slot);
            self.slots.remove(task_name);
        }
    }

    #[inline]
    fn get_or_create_slot(&self, slot_name: &str) -> Arc<Mutex<SlotState>> {
        self.slots
            .entry(slot_name.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(SlotState::new())))
            .clone()
    }
}
