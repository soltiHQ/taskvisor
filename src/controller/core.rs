//! # Controller: slot-based admission control for task submissions.
//!
//! [`Controller`] manages named slots, each running at most one task at a time.
//!
//! ## State machine (per slot)
//!
//! ```text
//! Idle ──submit──► Running ──TaskRemoved──► Idle (or start next from queue)
//!                     │
//!                     ├──Replace──► Terminating ──TaskRemoved──► Running (queued next)
//!                     │
//!                     └──DropIfRunning──► rejected
//! ```
//!
//! ## Architecture
//!
//! ```text
//! SupervisorHandle::submit()
//!   └─► ControllerHandle.tx ──mpsc──► Controller::run_inner()
//!                                       ├─► handle_submission() (admission logic)
//!                                       └─► handle_event() (TaskRemoved → free slot)
//! ```

use std::{
    sync::{Arc, Weak},
    time::Instant,
};

use dashmap::DashMap;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use tokio::sync::broadcast;

use crate::{
    Supervisor,
    events::{Bus, Event, EventKind},
};

use super::{
    admission::AdmissionPolicy,
    config::ControllerConfig,
    error::ControllerError,
    slot::{SlotState, SlotStatus},
    spec::ControllerSpec,
};

/// Handle for submitting tasks to the controller.
#[derive(Clone)]
pub(crate) struct ControllerHandle {
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
/// Each slot can run at most one task at a time.
/// New submissions are handled according to the configured [`AdmissionPolicy`].
/// See the [module-level documentation](self) for the state machine and architecture.
///
/// # Also
///
/// - [`ControllerSpec`] - submission request (admission + task spec)
/// - [`ControllerConfig`] - queue capacity and per-slot limits
/// - [`AdmissionPolicy`] - Queue / Replace / DropIfRunning
/// - [`SlotState`](super::slot::SlotState) - per-slot status and pending queue
pub(crate) struct Controller {
    config: ControllerConfig,
    supervisor: Weak<Supervisor>,
    bus: Bus,

    slots: DashMap<Arc<str>, Arc<Mutex<SlotState>>>,

    tx: mpsc::Sender<ControllerSpec>,
    rx: RwLock<Option<mpsc::Receiver<ControllerSpec>>>,
}

impl Controller {
    /// Creates a new controller *(must call .run() to start)*.
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
        let bus = self.bus.clone();
        tokio::spawn(async move {
            if let Err(e) = self.run_inner(token).await {
                bus.publish(
                    Event::new(EventKind::ControllerRejected)
                        .with_task("controller")
                        .with_reason(format!("controller_loop_exited: {e}")),
                );
            }
        });
    }

    /// Main event loop: receives submissions via mpsc, lifecycle events via bus.
    async fn run_inner(&self, token: CancellationToken) -> Result<(), ControllerError> {
        let mut rx = self
            .rx
            .write()
            .await
            .take()
            .ok_or(ControllerError::AlreadyRunning)?;

        let mut bus_rx = self.bus.subscribe();
        loop {
            tokio::select! {
                _ = token.cancelled() => break,

                Some(spec) = rx.recv() => {
                    self.handle_submission(spec).await;
                }
                result = bus_rx.recv() => {
                    match result {
                        Ok(event) => {
                            self.handle_event(event).await;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            self.bus.publish(
                                Event::new(EventKind::ControllerRejected)
                                    .with_task("controller")
                                    .with_reason(format!("bus_lagged: missed {n} events, recovering slots")),
                            );
                            self.recover_stale_slots().await;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
        Ok(())
    }

    /// Recovers slots that may be stuck in `Running`/`Terminating` after bus lag.
    ///
    /// When the broadcast receiver falls behind, we may have missed `TaskRemoved` events.
    /// Walk all slots and check if the task is still alive in the registry.
    /// If not, treat it as finished.
    async fn recover_stale_slots(&self) {
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };

        let slot_keys: Vec<Arc<str>> = self
            .slots
            .iter()
            .map(|entry| Arc::clone(entry.key()))
            .collect();

        for slot_name in slot_keys {
            let Some(slot_arc) = self.slots.get(&*slot_name).map(|e| e.clone()) else {
                continue;
            };
            let mut slot = slot_arc.lock().await;

            if matches!(slot.status, SlotStatus::Idle) {
                continue;
            }

            if !sup.registry_contains(&slot_name).await {
                slot.status = SlotStatus::Idle;

                if let Some(next_spec) = slot.queue.pop_front() {
                    if let Err(e) = sup.add_task(next_spec) {
                        self.bus.publish(
                            Event::new(EventKind::ControllerRejected)
                                .with_task(Arc::clone(&slot_name))
                                .with_reason(format!("recovery_start_failed: {e}")),
                        );
                        continue;
                    }
                    slot.status = SlotStatus::Running {
                        started_at: Instant::now(),
                    };
                    self.bus.publish(
                        Event::new(EventKind::ControllerSubmitted)
                            .with_task(Arc::clone(&slot_name))
                            .with_reason(format!("recovered_from_lag depth={}", slot.queue.len())),
                    );
                } else {
                    drop(slot);
                    self.slots.remove(&*slot_name);
                }
            }
        }
    }

    /// Handles a new task submission by applying admission policy to the slot.
    ///
    /// **Idle** (any policy): start immediately via `add_task`.
    ///
    /// **Queue**: push to tail (FIFO). Rejected if per-slot queue is full.
    ///
    /// **Replace** (latest-wins): replaces the queue head (immediate successor).
    /// - If `Running` → transition to `Terminating`, call `remove_task` once.
    /// - If already `Terminating` → replace head only, do NOT call remove again.
    /// - The next task actually starts in `on_task_finished` upon `TaskRemoved`.
    ///
    /// **DropIfRunning**: silently reject if slot is `Running` or `Terminating`.
    async fn handle_submission(&self, spec: ControllerSpec) {
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };

        let slot_name: Arc<str> = Arc::from(spec.slot_name());
        let admission = spec.admission;
        let task_spec = spec.task_spec;

        let slot_arc = self.get_or_create_slot(&slot_name);
        let mut slot = slot_arc.lock().await;

        match (&slot.status, admission) {
            (SlotStatus::Idle, _) => {
                if let Err(e) = sup.add_task(task_spec) {
                    self.bus.publish(
                        Event::new(EventKind::ControllerRejected)
                            .with_task(Arc::clone(&slot_name))
                            .with_reason(format!("add_failed: {}", e)),
                    );
                    return;
                }
                slot.status = SlotStatus::Running {
                    started_at: Instant::now(),
                };
                self.bus.publish(
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(Arc::clone(&slot_name))
                        .with_reason(format!("admission={:?} status=running", admission)),
                );
            }
            (SlotStatus::Running { .. }, AdmissionPolicy::Replace) => {
                Self::replace_head_or_push(&mut slot, task_spec);
                slot.status = SlotStatus::Terminating {
                    cancelled_at: Instant::now(),
                };
                self.bus.publish(
                    Event::new(EventKind::ControllerSlotTransition)
                        .with_task(Arc::clone(&slot_name))
                        .with_reason("running→terminating (replace)"),
                );
                if let Err(e) = sup.remove_task(&slot_name) {
                    self.bus.publish(
                        Event::new(EventKind::ControllerRejected)
                            .with_task(Arc::clone(&slot_name))
                            .with_reason(format!("remove_failed: {}", e)),
                    );
                }
                self.bus.publish(
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(Arc::clone(&slot_name))
                        .with_reason(format!("admission=Replace depth={}", slot.queue.len())),
                );
            }
            (SlotStatus::Running { .. }, AdmissionPolicy::Queue) => {
                if self.reject_if_full(&slot_name, slot.queue.len()) {
                    return;
                }
                slot.queue.push_back(task_spec);
                self.bus.publish(
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(Arc::clone(&slot_name))
                        .with_reason(format!("admission=Queue depth={}", slot.queue.len())),
                );
            }
            (SlotStatus::Terminating { .. }, AdmissionPolicy::Replace) => {
                Self::replace_head_or_push(&mut slot, task_spec);
                self.bus.publish(
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(Arc::clone(&slot_name))
                        .with_reason(format!(
                            "admission=Replace status=terminating depth={}",
                            slot.queue.len()
                        )),
                );
            }
            (SlotStatus::Terminating { .. }, AdmissionPolicy::Queue) => {
                if self.reject_if_full(&slot_name, slot.queue.len()) {
                    return;
                }
                slot.queue.push_back(task_spec);
                self.bus.publish(
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(Arc::clone(&slot_name))
                        .with_reason(format!(
                            "admission=Queue status=terminating depth={}",
                            slot.queue.len()
                        )),
                );
            }
            (SlotStatus::Running { .. }, AdmissionPolicy::DropIfRunning)
            | (SlotStatus::Terminating { .. }, AdmissionPolicy::DropIfRunning) => {
                self.bus.publish(
                    Event::new(EventKind::ControllerRejected)
                        .with_task(Arc::clone(&slot_name))
                        .with_reason(format!("dropped: slot busy (status={:?})", slot.status)),
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

        while let Some(next_spec) = slot.queue.pop_front() {
            match sup.add_task(next_spec) {
                Ok(()) => {
                    slot.status = SlotStatus::Running {
                        started_at: Instant::now(),
                    };
                    self.bus.publish(
                        Event::new(EventKind::ControllerSubmitted)
                            .with_task(task_name)
                            .with_reason(format!("started_from_queue depth={}", slot.queue.len())),
                    );
                    break;
                }
                Err(e) => {
                    self.bus.publish(
                        Event::new(EventKind::ControllerRejected)
                            .with_task(task_name)
                            .with_reason(format!("queue_start_failed: {e}")),
                    );
                }
            }
        }
        if matches!(slot.status, SlotStatus::Idle) && slot.queue.is_empty() {
            drop(slot);
            self.slots.remove(task_name);
        }
    }

    #[inline]
    fn get_or_create_slot(&self, slot_name: &str) -> Arc<Mutex<SlotState>> {
        if let Some(slot) = self.slots.get(slot_name) {
            return slot.clone();
        }
        self.slots
            .entry(Arc::from(slot_name))
            .or_insert_with(|| Arc::new(Mutex::new(SlotState::new())))
            .clone()
    }

    #[inline]
    fn reject_if_full(&self, slot_name: &str, slot_len: usize) -> bool {
        if slot_len >= self.config.max_slot_queue {
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task(slot_name)
                    .with_reason(format!(
                        "queue_full: {}/{}",
                        slot_len, self.config.max_slot_queue
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackoffPolicy, RestartPolicy, TaskFn, TaskRef, TaskSpec};
    use tokio_util::sync::CancellationToken;

    fn make_spec(name: &str) -> TaskSpec {
        let task: TaskRef = TaskFn::arc(name, |_ctx: CancellationToken| async { Ok(()) });
        TaskSpec::new(task, RestartPolicy::Never, BackoffPolicy::default(), None)
    }

    #[test]
    fn replace_head_or_push_into_empty_queue() {
        let mut slot = SlotState::new();
        Controller::replace_head_or_push(&mut slot, make_spec("first"));

        assert_eq!(slot.queue.len(), 1);
        assert_eq!(slot.queue.front().unwrap().name(), "first");
    }

    #[test]
    fn replace_head_or_push_replaces_existing_head() {
        let mut slot = SlotState::new();
        slot.queue.push_back(make_spec("old-head"));
        slot.queue.push_back(make_spec("tail"));

        Controller::replace_head_or_push(&mut slot, make_spec("new-head"));

        assert_eq!(slot.queue.len(), 2, "queue depth should not grow");
        assert_eq!(slot.queue.front().unwrap().name(), "new-head");
        assert_eq!(slot.queue.back().unwrap().name(), "tail");
    }

    #[test]
    fn replace_head_multiple_times_keeps_depth_1() {
        let mut slot = SlotState::new();
        Controller::replace_head_or_push(&mut slot, make_spec("v1"));
        Controller::replace_head_or_push(&mut slot, make_spec("v2"));
        Controller::replace_head_or_push(&mut slot, make_spec("v3"));

        assert_eq!(slot.queue.len(), 1);
        assert_eq!(slot.queue.front().unwrap().name(), "v3");
    }

    #[test]
    fn reject_if_full_returns_false_below_capacity() {
        let bus = Bus::new(64);
        let config = ControllerConfig {
            queue_capacity: 16,
            max_slot_queue: 3,
        };
        let ctrl = make_controller(config, bus);
        assert!(!ctrl.reject_if_full("slot", 0));
        assert!(!ctrl.reject_if_full("slot", 2));
    }

    #[test]
    fn reject_if_full_returns_true_at_capacity() {
        let bus = Bus::new(64);
        let config = ControllerConfig {
            queue_capacity: 16,
            max_slot_queue: 3,
        };
        let ctrl = make_controller(config, bus);
        assert!(ctrl.reject_if_full("slot", 3));
        assert!(ctrl.reject_if_full("slot", 10));
    }

    #[test]
    fn get_or_create_slot_creates_idle_slot() {
        let bus = Bus::new(64);
        let ctrl = make_controller(ControllerConfig::default(), bus);

        let slot_arc = ctrl.get_or_create_slot("my-slot");
        let slot = slot_arc.blocking_lock();
        assert_eq!(slot.status, SlotStatus::Idle);
        assert!(slot.queue.is_empty());
    }

    #[test]
    fn get_or_create_slot_returns_same_arc() {
        let bus = Bus::new(64);
        let ctrl = make_controller(ControllerConfig::default(), bus);

        let s1 = ctrl.get_or_create_slot("x");
        let s2 = ctrl.get_or_create_slot("x");
        assert!(Arc::ptr_eq(&s1, &s2), "same slot name must return same Arc");
    }

    #[test]
    fn get_or_create_slot_different_names_different_arcs() {
        let bus = Bus::new(64);
        let ctrl = make_controller(ControllerConfig::default(), bus);

        let s1 = ctrl.get_or_create_slot("a");
        let s2 = ctrl.get_or_create_slot("b");
        assert!(!Arc::ptr_eq(&s1, &s2));
    }

    #[tokio::test]
    async fn handle_event_ignores_non_task_removed() {
        let bus = Bus::new(64);
        let ctrl = make_controller(ControllerConfig::default(), bus);

        let slot_arc = ctrl.get_or_create_slot("t");
        {
            let mut slot = slot_arc.lock().await;
            slot.status = SlotStatus::Running {
                started_at: Instant::now(),
            };
        }

        let event = Arc::new(Event::new(EventKind::TaskFailed).with_task("t"));
        ctrl.handle_event(event).await;

        let slot = slot_arc.lock().await;
        assert!(
            matches!(slot.status, SlotStatus::Running { .. }),
            "non-TaskRemoved events should not affect slot state"
        );
    }

    fn make_controller(config: ControllerConfig, bus: Bus) -> Controller {
        let (tx, rx) = mpsc::channel(config.queue_capacity);
        Controller {
            config,
            supervisor: Weak::new(),
            bus,
            slots: DashMap::new(),
            tx,
            rx: RwLock::new(Some(rx)),
        }
    }
}
