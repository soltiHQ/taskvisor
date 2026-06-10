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
    time::{Duration, Instant},
};

use dashmap::DashMap;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use tokio::sync::broadcast;

use crate::{
    RuntimeError, Supervisor, TaskSpec,
    events::{Bus, Event, EventKind},
    identity::TaskId,
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
    running: DashMap<TaskId, Arc<str>>,

    tx: mpsc::Sender<ControllerSpec>,
    rx: RwLock<Option<mpsc::Receiver<ControllerSpec>>>,

    shutting_down: std::sync::atomic::AtomicBool,
}

impl Controller {
    /// Creates a new controller (must call [`run`](Self::run) to start it).
    pub fn new(config: ControllerConfig, supervisor: &Arc<Supervisor>, bus: Bus) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(config.queue_capacity.max(1));

        Arc::new(Self {
            config,
            supervisor: Arc::downgrade(supervisor),
            bus,
            slots: DashMap::new(),
            running: DashMap::new(),
            tx,
            rx: RwLock::new(Some(rx)),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Returns `true` once shutdown has been observed on the bus.
    fn is_shutting_down(&self) -> bool {
        self.shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
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

    /// Recovers slots wedged by a bus lag (a missed `TaskAdded`/`TaskRemoved`).
    async fn recover_stale_slots(&self) {
        const RECOVERY_DEADLINE: Duration = Duration::from_secs(5);

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
            match slot.status {
                SlotStatus::Admitting { since } if since.elapsed() < RECOVERY_DEADLINE => continue,
                SlotStatus::Terminating { cancelled_at }
                    if cancelled_at.elapsed() < RECOVERY_DEADLINE =>
                {
                    continue;
                }
                _ => {}
            }

            let alive = match slot.running_id {
                Some(rid) => sup.contains_id(rid).await,
                None => false,
            };
            if alive {
                match slot.status {
                    SlotStatus::Admitting { .. } => {
                        slot.status = SlotStatus::Running {
                            started_at: Instant::now(),
                        };
                        self.bus.publish(
                            Event::new(EventKind::ControllerSlotTransition)
                                .with_task(Arc::clone(&slot_name))
                                .with_reason("admitting→running (lag recovery)"),
                        );
                    }
                    SlotStatus::Terminating { .. } => {
                        if let Some(rid) = slot.running_id
                            && let Err(e) = sup.remove(rid)
                        {
                            self.bus.publish(
                                Event::new(EventKind::ControllerRejected)
                                    .with_task(Arc::clone(&slot_name))
                                    .with_reason(format!("recovery_remove_failed: {e}")),
                            );
                        }
                    }
                    _ => {}
                }
                continue;
            }

            if let Some(id) = slot.running_id.take() {
                self.running.remove(&id);
            }
            slot.status = SlotStatus::Idle;

            if !self.is_shutting_down() {
                self.start_next_from_queue(&sup, &mut slot, &slot_name);
            }

            if matches!(slot.status, SlotStatus::Idle) && slot.queue.is_empty() {
                drop(slot);
                self.slots.remove(&*slot_name);
            }
        }
    }

    /// Applies the admission policy for a new submission.
    ///
    /// **Idle**: admit immediately, `add_task` mints the [`TaskId`] and the slot enters `Admitting`;
    /// it becomes `Running` only when the matching `TaskAdded` is observed.
    ///
    /// **Queue**: push to tail (FIFO); rejected if the per-slot queue is full.
    ///
    /// **Replace** (latest-wins): replaces the queue head;
    /// if the slot is `Running` the current task is removed now,
    /// if `Admitting` it is removed once it appears (in [`on_task_added`](Self::on_task_added)).
    ///
    /// **DropIfRunning**: reject while the slot is busy (`Admitting`/`Running`/`Terminating`).
    async fn handle_submission(&self, spec: ControllerSpec) {
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };
        if self.is_shutting_down() {
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task(spec.slot_name().to_owned())
                    .with_reason("controller_shutting_down"),
            );
            return;
        }

        let slot_name: Arc<str> = Arc::from(spec.slot_name());
        let admission = spec.admission;
        let task_spec = spec.task_spec;

        let slot_arc = self.get_or_create_slot(&slot_name);
        let mut slot = slot_arc.lock().await;

        match (&slot.status, admission) {
            (SlotStatus::Idle, _) => {
                match self.start_in_slot(&sup, &mut slot, &slot_name, task_spec) {
                    Ok(_id) => {
                        self.bus.publish(
                            Event::new(EventKind::ControllerSubmitted)
                                .with_task(Arc::clone(&slot_name))
                                .with_reason(format!("admission={admission:?} status=admitting")),
                        );
                    }
                    Err(e) => {
                        self.bus.publish(
                            Event::new(EventKind::ControllerRejected)
                                .with_task(Arc::clone(&slot_name))
                                .with_reason(format!("add_failed: {e}")),
                        );
                        drop(slot);
                        self.slots.remove(&*slot_name);
                    }
                }
            }
            (SlotStatus::Running { .. }, AdmissionPolicy::Replace) => {
                if let Some(rid) = slot.running_id
                    && let Err(e) = sup.remove(rid)
                {
                    self.bus.publish(
                        Event::new(EventKind::ControllerRejected)
                            .with_task(Arc::clone(&slot_name))
                            .with_reason(format!("remove_failed: {e}")),
                    );
                    return;
                }
                Self::replace_head_or_push(&mut slot, task_spec);
                slot.status = SlotStatus::Terminating {
                    cancelled_at: Instant::now(),
                };
                self.bus.publish(
                    Event::new(EventKind::ControllerSlotTransition)
                        .with_task(Arc::clone(&slot_name))
                        .with_reason("running→terminating (replace)"),
                );
                self.bus.publish(
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(Arc::clone(&slot_name))
                        .with_reason(format!("admission=Replace depth={}", slot.queue.len())),
                );
            }
            (SlotStatus::Admitting { .. }, AdmissionPolicy::Replace) => {
                Self::replace_head_or_push(&mut slot, task_spec);
                slot.status = SlotStatus::Terminating {
                    cancelled_at: Instant::now(),
                };
                self.bus.publish(
                    Event::new(EventKind::ControllerSlotTransition)
                        .with_task(Arc::clone(&slot_name))
                        .with_reason("admitting→terminating (replace)"),
                );
                self.bus.publish(
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(Arc::clone(&slot_name))
                        .with_reason(format!(
                            "admission=Replace status=admitting depth={}",
                            slot.queue.len()
                        )),
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
            (
                SlotStatus::Admitting { .. }
                | SlotStatus::Running { .. }
                | SlotStatus::Terminating { .. },
                AdmissionPolicy::Queue,
            ) => {
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
            (
                SlotStatus::Admitting { .. }
                | SlotStatus::Running { .. }
                | SlotStatus::Terminating { .. },
                AdmissionPolicy::DropIfRunning,
            ) => {
                self.bus.publish(
                    Event::new(EventKind::ControllerRejected)
                        .with_task(Arc::clone(&slot_name))
                        .with_reason(format!("dropped: slot busy (status={:?})", slot.status)),
                );
            }
        }
    }

    /// Routes bus events that drive slot transitions, correlated by [`TaskId`].
    async fn handle_event(&self, event: Arc<Event>) {
        match event.kind {
            EventKind::TaskAdded => self.on_task_added(&event).await,
            EventKind::TaskRemoved => self.on_task_finished(&event).await,
            EventKind::TaskAddFailed => self.on_task_add_failed(&event).await,
            EventKind::ShutdownRequested => {
                self.shutting_down
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            _ => {}
        }
    }

    /// `TaskAdded`: the admitted actor now exists - flip `Admitting` → `Running`.
    /// If a `Replace` arrived while admitting (slot is `Terminating`), remove it now.
    async fn on_task_added(&self, event: &Event) {
        let Some(id) = event.id else {
            return;
        };
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };
        let Some(slot_name) = self.running.get(&id).map(|e| e.clone()) else {
            return;
        };
        let Some(slot_arc) = self.slots.get(&*slot_name).map(|e| e.clone()) else {
            return;
        };
        let mut slot = slot_arc.lock().await;
        if slot.running_id != Some(id) {
            return;
        }
        match slot.status {
            SlotStatus::Admitting { .. } => {
                slot.status = SlotStatus::Running {
                    started_at: Instant::now(),
                };
                self.bus.publish(
                    Event::new(EventKind::ControllerSlotTransition)
                        .with_task(slot_name)
                        .with_reason("admitting→running"),
                );
            }
            SlotStatus::Terminating { .. } => {
                if let Some(rid) = slot.running_id
                    && let Err(e) = sup.remove(rid)
                {
                    self.bus.publish(
                        Event::new(EventKind::ControllerRejected)
                            .with_task(slot_name)
                            .with_reason(format!("remove_failed: {e}")),
                    );
                }
            }
            _ => {}
        }
    }

    /// `TaskAddFailed`: admission was rejected (e.g. duplicate name)
    /// free the slot and start the next queued task, so a slot can never wedge in `Admitting`.
    async fn on_task_add_failed(&self, event: &Event) {
        self.free_and_advance(event).await;
    }

    /// `TaskRemoved`: the occupying task ended - free the slot and start the next queued task.
    /// Correlating by [`TaskId`] makes duplicate/stale removals harmless no-ops.
    async fn on_task_finished(&self, event: &Event) {
        self.free_and_advance(event).await;
    }

    /// Common terminal handling for `TaskRemoved`/`TaskAddFailed`:
    /// resolve the owning slot by [`TaskId`], free it, and advance its queue.
    ///
    /// Idempotent: a stale/duplicate event whose id is no longer in the index is a no-op.
    async fn free_and_advance(&self, event: &Event) {
        let Some(id) = event.id else {
            return;
        };
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };
        let Some((_, slot_name)) = self.running.remove(&id) else {
            return;
        };
        let Some(slot_arc) = self.slots.get(&*slot_name).map(|e| e.clone()) else {
            return;
        };
        let mut slot = slot_arc.lock().await;
        if slot.running_id != Some(id) {
            return;
        }

        slot.running_id = None;
        slot.status = SlotStatus::Idle;

        if !self.is_shutting_down() {
            self.start_next_from_queue(&sup, &mut slot, &slot_name);
        }

        if matches!(slot.status, SlotStatus::Idle) && slot.queue.is_empty() {
            drop(slot);
            self.slots.remove(&*slot_name);
        }
    }

    /// Admits `task_spec` into the slot: mints the identity via `add_task`, marks the slot `Admitting`, and records the reverse index.
    /// The slot becomes `Running` on `TaskAdded`.
    fn start_in_slot(
        &self,
        sup: &Arc<Supervisor>,
        slot: &mut SlotState,
        slot_name: &Arc<str>,
        task_spec: TaskSpec,
    ) -> Result<TaskId, RuntimeError> {
        let id = sup.add_task(task_spec)?;
        slot.status = SlotStatus::Admitting {
            since: Instant::now(),
        };
        slot.running_id = Some(id);
        self.running.insert(id, Arc::clone(slot_name));
        Ok(id)
    }

    /// Pops and admits queued tasks until one is accepted or the queue drains.
    /// Leaves the slot `Admitting` (one started) or `Idle` (queue empty / all failed).
    fn start_next_from_queue(
        &self,
        sup: &Arc<Supervisor>,
        slot: &mut SlotState,
        slot_name: &Arc<str>,
    ) {
        while let Some(next_spec) = slot.queue.pop_front() {
            match self.start_in_slot(sup, slot, slot_name, next_spec) {
                Ok(_id) => {
                    self.bus.publish(
                        Event::new(EventKind::ControllerSubmitted)
                            .with_task(Arc::clone(slot_name))
                            .with_reason(format!("started_from_queue depth={}", slot.queue.len())),
                    );
                    return;
                }
                Err(e) => {
                    self.bus.publish(
                        Event::new(EventKind::ControllerRejected)
                            .with_task(Arc::clone(slot_name))
                            .with_reason(format!("queue_start_failed: {e}")),
                    );
                }
            }
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
        let (tx, rx) = mpsc::channel(config.queue_capacity.max(1));
        Controller {
            config,
            supervisor: Weak::new(),
            bus,
            slots: DashMap::new(),
            running: DashMap::new(),
            tx,
            rx: RwLock::new(Some(rx)),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
        }
    }

    #[tokio::test]
    async fn zero_queue_capacity_is_clamped_not_panicking() {
        let sup = Supervisor::builder(crate::SupervisorConfig::default())
            .with_controller(ControllerConfig {
                queue_capacity: 0,
                max_slot_queue: 1,
            })
            .build();
        let handle = sup.serve();

        let task: TaskRef = TaskFn::arc("clamped", |_ctx: CancellationToken| async { Ok(()) });
        handle
            .submit(ControllerSpec::queue(TaskSpec::once(task)))
            .await
            .expect("submission must work with capacity clamped to 1");

        let _ = handle.shutdown().await;
    }

    fn long_ago() -> Instant {
        Instant::now()
            .checked_sub(Duration::from_secs(60))
            .expect("test host uptime must exceed one minute")
    }

    async fn sup_with_live_task() -> (Arc<Supervisor>, crate::core::SupervisorHandle, TaskId) {
        let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
        let handle = sup.serve();
        let task: TaskRef = TaskFn::arc("occupant", |ctx: CancellationToken| async move {
            ctx.cancelled().await;
            Ok(())
        });
        let id = handle
            .add_and_wait(TaskSpec::restartable(task), Duration::from_secs(1))
            .await
            .expect("task should register");
        (sup, handle, id)
    }

    #[tokio::test]
    async fn recover_promotes_admitting_slot_with_alive_task() {
        let (sup, handle, id) = sup_with_live_task().await;
        let ctrl = Controller::new(ControllerConfig::default(), &sup, Bus::new(64));

        ctrl.slots.insert(
            Arc::from("s"),
            Arc::new(Mutex::new(SlotState {
                status: SlotStatus::Admitting { since: long_ago() },
                running_id: Some(id),
                queue: std::collections::VecDeque::new(),
            })),
        );
        ctrl.running.insert(id, Arc::from("s"));

        ctrl.recover_stale_slots().await;

        let slot_arc = ctrl.slots.get("s").map(|e| e.clone()).expect("slot exists");
        let slot = slot_arc.lock().await;
        assert!(
            matches!(slot.status, SlotStatus::Running { .. }),
            "an Admitting slot whose task is alive must be promoted to Running, got {:?}",
            slot.status
        );

        drop(slot);
        let _ = handle.shutdown().await;
    }

    #[tokio::test]
    async fn recover_reissues_removal_for_terminating_slot_with_alive_task() {
        let (sup, handle, id) = sup_with_live_task().await;
        let ctrl = Controller::new(ControllerConfig::default(), &sup, Bus::new(64));

        ctrl.slots.insert(
            Arc::from("s"),
            Arc::new(Mutex::new(SlotState {
                status: SlotStatus::Terminating {
                    cancelled_at: long_ago(),
                },
                running_id: Some(id),
                queue: std::collections::VecDeque::new(),
            })),
        );
        ctrl.running.insert(id, Arc::from("s"));

        ctrl.recover_stale_slots().await;

        let removed = poll_until(Duration::from_secs(2), || async {
            !sup.contains_id(id).await
        })
        .await;
        assert!(
            removed,
            "recovery must re-issue the deferred removal for a Terminating slot"
        );

        let _ = handle.shutdown().await;
    }

    #[tokio::test]
    async fn no_queue_advancement_after_shutdown_requested() {
        let (sup, handle, id) = sup_with_live_task().await;
        let ctrl = Controller::new(ControllerConfig::default(), &sup, Bus::new(64));

        let queued: TaskRef = TaskFn::arc("queued", |ctx: CancellationToken| async move {
            ctx.cancelled().await;
            Ok(())
        });
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(TaskSpec::restartable(queued));
        ctrl.slots.insert(
            Arc::from("s"),
            Arc::new(Mutex::new(SlotState {
                status: SlotStatus::Running {
                    started_at: Instant::now(),
                },
                running_id: Some(id),
                queue,
            })),
        );
        ctrl.running.insert(id, Arc::from("s"));
        ctrl.handle_event(Arc::new(Event::new(EventKind::ShutdownRequested)))
            .await;
        ctrl.handle_event(Arc::new(
            Event::new(EventKind::TaskRemoved)
                .with_task("occupant")
                .with_id(id),
        ))
        .await;

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            sup.id_for_label("queued").await.is_none(),
            "controller must not start queued tasks once shutdown has been requested"
        );

        let _ = handle.shutdown().await;
    }

    #[tokio::test]
    async fn replace_supersedes_in_same_slot() {
        let sup = Supervisor::builder(crate::SupervisorConfig::default())
            .with_controller(ControllerConfig::default())
            .build();
        let handle = sup.serve();

        let mk = |name: &'static str| -> ControllerSpec {
            let task: TaskRef = TaskFn::arc(name, |ctx: CancellationToken| async move {
                ctx.cancelled().await;
                Ok(())
            });
            ControllerSpec::replace(TaskSpec::restartable(task).with_slot("s"))
        };

        handle.submit(mk("run-1")).await.unwrap();
        handle.submit(mk("run-2")).await.unwrap();

        let superseded = poll_until(std::time::Duration::from_secs(3), || async {
            let alive = handle.snapshot().await;
            alive.iter().any(|n| &**n == "run-2") && alive.iter().all(|n| &**n != "run-1")
        })
        .await;
        assert!(
            superseded,
            "Replace must supersede run-1 with run-2 in the shared slot, not run both"
        );

        let _ = handle.shutdown().await;
    }

    async fn poll_until<F, Fut>(within: std::time::Duration, mut cond: F) -> bool
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            if cond().await {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
