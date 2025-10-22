use std::{
    sync::{Arc, Weak},
    time::Instant,
};

use dashmap::DashMap;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{
    Supervisor,
    events::{Bus, Event, EventKind},
};

use super::{
    admission::ControllerAdmission,
    config::ControllerConfig,
    error::ControllerError,
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
    pub async fn submit(&self, spec: ControllerSpec) -> Result<(), ControllerError> {
        self.tx
            .send(spec)
            .await
            .map_err(|_| ControllerError::Closed)
    }

    /// Try to submit without blocking (fails if queue full).
    pub fn try_submit(&self, spec: ControllerSpec) -> Result<(), ControllerError> {
        self.tx.try_send(spec).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => ControllerError::Full,
            mpsc::error::TrySendError::Closed(_) => ControllerError::Closed,
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
    /// Replace semantics (latest-wins):
    /// - Replace does NOT grow the queue; it replaces the head (the immediate successor).
    /// - If slot was Running → transition to Terminating and request remove once.
    /// - If already Terminating → do NOT call remove again; just replace the head.
    /// - The next task actually starts in `on_task_finished` upon terminal event.
    async fn handle_submission(&self, spec: ControllerSpec) {
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };

        let slot_name = spec.slot_name().to_string();
        let admission = spec.admission;
        let task_spec = spec.task_spec;

        let slot_arc = self.get_or_create_slot(&slot_name);
        let mut slot = slot_arc.lock().await;

        match (&slot.status, admission) {
            (SlotStatus::Idle, _) => {
                if let Err(e) = sup.add_task(task_spec) {
                    self.bus.publish(
                        Event::new(EventKind::ControllerRejected)
                            .with_task(slot_name.as_str())
                            .with_reason(format!("add_failed: {}", e)),
                    );
                    return;
                }
                slot.status = SlotStatus::Running {
                    started_at: Instant::now(),
                };
                self.bus.publish(
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(slot_name.as_str())
                        .with_reason(format!("admission={:?} status=running", admission)),
                );
            }
            (SlotStatus::Running { .. }, ControllerAdmission::Replace) => {
                Self::replace_head_or_push(&mut slot, task_spec);
                slot.status = SlotStatus::Terminating {
                    cancelled_at: Instant::now(),
                };
                self.bus.publish(
                    Event::new(EventKind::ControllerSlotTransition)
                        .with_task(slot_name.as_str())
                        .with_reason("running→terminating (replace)"),
                );
                if let Err(e) = sup.remove_task(&slot_name) {
                    self.bus.publish(
                        Event::new(EventKind::ControllerRejected)
                            .with_task(slot_name.as_str())
                            .with_reason(format!("remove_failed: {}", e)),
                    );
                }
                self.bus.publish(
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(slot_name.as_str())
                        .with_reason(format!("admission=Replace depth={}", slot.queue.len())),
                );
            }
            (SlotStatus::Running { .. }, ControllerAdmission::Queue) => {
                if self.reject_if_full(&slot_name, slot.queue.len()) {
                    return;
                }
                slot.queue.push_back(task_spec);
                self.bus.publish(
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(slot_name.as_str())
                        .with_reason(format!("admission=Queue depth={}", slot.queue.len())),
                );
            }
            (SlotStatus::Terminating { .. }, ControllerAdmission::Replace) => {
                Self::replace_head_or_push(&mut slot, task_spec);
                self.bus.publish(
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(slot_name.as_str())
                        .with_reason(format!(
                            "admission=Replace status=terminating depth={}",
                            slot.queue.len()
                        )),
                );
            }
            (SlotStatus::Terminating { .. }, ControllerAdmission::Queue) => {
                if self.reject_if_full(&slot_name, slot.queue.len()) {
                    return;
                }
                slot.queue.push_back(task_spec);
                self.bus.publish(
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(slot_name.as_str())
                        .with_reason(format!(
                            "admission=Queue status=terminating depth={}",
                            slot.queue.len()
                        )),
                );
            }
            (SlotStatus::Running { .. }, ControllerAdmission::DropIfRunning) => {}
            _ => {
                if self.reject_if_full(&slot_name, slot.queue.len()) {
                    return;
                }
                slot.queue.push_back(task_spec);
                self.bus.publish(
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(slot_name.as_str())
                        .with_reason(format!(
                            "admission={:?} depth={}",
                            admission,
                            slot.queue.len()
                        )),
                );
            }
        }
    }

    /// Handles bus events (terminal only).
    async fn handle_event(&self, event: Arc<Event>) {
        if event.kind == EventKind::TaskRemoved {
            self.on_task_finished(&event).await
        }
    }

    /// Handles `TaskRemoved` for a task; frees the slot and optionally starts the queued next.
    ///
    /// Rationale: `ActorExhausted`/`ActorDead` may arrive before the actor is
    /// deregistered in the Supervisor’s Registry. Starting the next task before
    /// `TaskRemoved` can race and produce `task_already_exists`. By gating on
    /// `TaskRemoved` we avoid double-adds and registry races.
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

            self.bus.publish(
                Event::new(EventKind::ControllerSubmitted)
                    .with_task(task_name)
                    .with_reason(format!("started_from_queue depth={}", slot.queue.len())),
            );
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

    #[inline]
    fn reject_if_full(&self, slot_name: &str, slot_len: usize) -> bool {
        if slot_len >= self.config.slot_capacity {
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task(slot_name)
                    .with_reason(format!(
                        "queue_full: {}/{}",
                        slot_len, self.config.slot_capacity
                    )),
            );
            true
        } else {
            false
        }
    }

    #[inline]
    fn replace_head_or_push(slot: &mut SlotState, task_spec: crate::TaskSpec) {
        if let Some(head) = slot.queue.front_mut() {
            *head = task_spec;
        } else {
            slot.queue.push_front(task_spec);
        }
    }
}
