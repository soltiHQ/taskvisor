//! Owns the worker-thread queue for isolated user destruction.
//!
//! [`WorkerQueue`] accepts charged [`DropBatch`] values. Persistent core workers wait for work.
//! The queue attempts to start a temporary elastic worker when queued work has no idle worker.
//! Started elastic workers exit after an idle timeout.
//!
//! Worker creation happens outside the queue mutex.
//! A worker removes one batch under the mutex, releases it, then runs user destructors.

use std::{
    collections::VecDeque,
    io,
    num::NonZeroUsize,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{Receiver, channel},
    },
    thread::JoinHandle,
    time::Duration,
};

use super::batch::DropBatch;
use crate::core::deferred_drop::error::DropStartError;

/// Persistent workers that preserve progress past two blocked destructors.
pub(in crate::core::deferred_drop) const CORE_WORKER_COUNT: usize = 3;

/// Maximum live and starting cleanup workers in one domain.
pub(in crate::core::deferred_drop) const MAX_WORKER_COUNT: usize = 16;

/// Idle duration after which one elastic worker exits.
pub(in crate::core::deferred_drop) const ELASTIC_IDLE_TIMEOUT: Duration = Duration::from_secs(1);

/// Backlog delay before one elastic worker may request further expansion.
const ELASTIC_EXPANSION_DELAY: Duration = Duration::from_millis(10);

/// Operating-system thread factory replaceable by deterministic tests.
pub(in crate::core::deferred_drop) type WorkerSpawner =
    dyn Fn(usize, Receiver<WorkerLaunch>) -> io::Result<JoinHandle<()>> + Send + Sync + 'static;

/// Mutable queue and worker counts guarded by one mutex.
struct WorkerState {
    /// Charged cleanup batches in first-in, first-out order.
    queue: VecDeque<DropBatch>,
    /// Whether new batches are rejected and workers should exit.
    closed: bool,
    /// Activated workers that have not exited.
    live_workers: usize,
    /// Live workers currently waiting for work.
    idle_workers: usize,
    /// Spawned workers that have not entered queue accounting.
    starting_workers: usize,
    /// Batches removed from the queue whose user destructors have not returned.
    running_batches: usize,
}

/// Point-in-time deferred-cleanup queue state.
pub(in crate::core::deferred_drop) struct CleanupSnapshot {
    /// Charged batches waiting for a worker.
    pub(in crate::core::deferred_drop) queued: usize,
    /// Charged batches claimed by workers and not yet completed.
    pub(in crate::core::deferred_drop) running: usize,
}

/// Cleanup queue with persistent core and temporary elastic workers.
pub(in crate::core::deferred_drop) struct WorkerQueue {
    /// First-in, first-out contents and worker accounting.
    state: Mutex<WorkerState>,
    /// Wakes idle workers after queue and lifecycle changes.
    ready: Condvar,
    /// Creates operating-system cleanup threads.
    spawner: Arc<WorkerSpawner>,
    /// Persistent workers created transactionally at domain startup.
    core_workers: usize,
    /// Hard ceiling for live and starting workers.
    max_workers: usize,
    /// Worker index assigned to the next elastic startup.
    next_worker: AtomicUsize,
}

impl WorkerQueue {
    /// Creates an empty queue with worker limits and no started threads.
    pub(super) fn new(
        core_workers: usize,
        max_workers: usize,
        spawner: Arc<WorkerSpawner>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(WorkerState {
                queue: VecDeque::new(),
                closed: false,
                live_workers: 0,
                idle_workers: 0,
                starting_workers: 0,
                running_batches: 0,
            }),
            ready: Condvar::new(),
            spawner,
            core_workers,
            max_workers,
            next_worker: AtomicUsize::new(0),
        })
    }

    /// Activates the complete core set before external submission can begin.
    ///
    /// # Errors
    ///
    /// Returns an error when a core thread cannot be created or does not finish its startup handshake.
    pub(super) fn start_core(self: &Arc<Self>, count: usize) -> Result<(), DropStartError> {
        let mut launchers = Vec::with_capacity(count);
        for worker in 0..count {
            let (launch, launcher) = channel();
            match (self.spawner)(worker, launcher) {
                Ok(thread) => launchers.push((Some(launch), thread)),
                Err(source) => {
                    drop(launch);
                    for (launch, thread) in launchers {
                        drop(launch);
                        let _ = thread.join();
                    }
                    return Err(DropStartError::new(worker, source));
                }
            }
        }

        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.starting_workers = count;
        }
        self.next_worker.store(count, Ordering::Release);
        for worker in 0..launchers.len() {
            let (started, started_rx) = channel();
            let launch = launchers[worker]
                .0
                .take()
                .expect("each core worker has exactly one startup gate");
            if launch
                .send(WorkerLaunch {
                    queue: Arc::clone(self),
                    core: true,
                    started: Some(started),
                })
                .is_err()
                || started_rx.recv().is_err()
            {
                self.close();
                for (launch, thread) in launchers {
                    drop(launch);
                    let _ = thread.join();
                }
                return Err(DropStartError::new(
                    worker,
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "destructor-isolation worker exited during startup",
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Enqueues one charged batch and reserves at most one elastic startup.
    ///
    /// # Errors
    ///
    /// Returns the unchanged batch when the worker queue is closed.
    pub(super) fn submit(self: &Arc<Self>, batch: DropBatch) -> Result<(), DropBatch> {
        let spawn_worker = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.closed {
                return Err(batch);
            }
            state.queue.push_back(batch);
            let spawn = self.reserve_elastic_locked(&mut state);
            self.ready.notify_one();
            spawn
        };

        if spawn_worker {
            self.spawn_elastic();
        }
        Ok(())
    }

    /// Charges one starting-worker count when current backlog needs expansion.
    fn reserve_elastic_locked(&self, state: &mut WorkerState) -> bool {
        let spawn = self.needs_elastic_locked(state);
        if spawn {
            state.starting_workers += 1;
        }
        spawn
    }

    /// Checks for queued work with no idle or already-starting worker.
    fn needs_elastic_locked(&self, state: &WorkerState) -> bool {
        !state.closed
            && !state.queue.is_empty()
            && state.idle_workers == 0
            && state.starting_workers == 0
            && state.live_workers < self.max_workers
    }

    /// Attempts one previously reserved elastic startup outside the queue mutex.
    ///
    /// Failure keeps the charged batch queued.
    /// A later submission or worker exit can retry expansion.
    fn spawn_elastic(self: &Arc<Self>) {
        let worker = self.next_worker.fetch_add(1, Ordering::AcqRel);
        let (launch, launcher) = channel();
        match (self.spawner)(worker, launcher) {
            Ok(thread) => {
                let closed = self
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .closed;
                if closed {
                    self.cancel_starting_worker();
                    drop(launch);
                    let _ = thread.join();
                    return;
                }
                if launch
                    .send(WorkerLaunch {
                        queue: Arc::clone(self),
                        core: false,
                        started: None,
                    })
                    .is_err()
                {
                    self.cancel_starting_worker();
                    let _ = thread.join();
                }
            }
            Err(_) => self.cancel_starting_worker(),
        }
    }

    /// Removes a failed or canceled elastic startup from accounting.
    fn cancel_starting_worker(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.starting_workers = state.starting_workers.saturating_sub(1);
    }

    /// Activates a worker and returns its next batch or exit decision.
    ///
    /// Core workers wait until work or closure. Elastic workers leave after an idle timeout.
    /// A worker may reserve the next elastic startup before it releases the queue mutex.
    fn take(
        &self,
        core: bool,
        active: &mut bool,
        started: &mut Option<std::sync::mpsc::Sender<()>>,
    ) -> WorkerTake {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !*active {
            state.starting_workers = state.starting_workers.saturating_sub(1);
            state.live_workers += 1;
            *active = true;
        }
        loop {
            if let Some(batch) = state.queue.pop_front() {
                state.running_batches += 1;
                if !core {
                    let (next, _) = self
                        .ready
                        .wait_timeout_while(state, ELASTIC_EXPANSION_DELAY, |state| {
                            self.needs_elastic_locked(state)
                        })
                        .unwrap_or_else(|error| error.into_inner());
                    state = next;
                }
                let spawn_elastic =
                    if !core || state.live_workers + state.starting_workers <= self.core_workers {
                        self.reserve_elastic_locked(&mut state)
                    } else {
                        false
                    };
                self.ready.notify_all();
                return WorkerTake::Batch {
                    batch,
                    spawn_elastic,
                };
            }
            if state.closed {
                return WorkerTake::Exit;
            }

            state.idle_workers += 1;
            if state.idle_workers == 1 {
                self.ready.notify_all();
            }
            if let Some(started) = started.take() {
                let _ = started.send(());
            }
            if core {
                state = self
                    .ready
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner());
                state.idle_workers = state.idle_workers.saturating_sub(1);
            } else {
                let (next, timeout) = self
                    .ready
                    .wait_timeout(state, ELASTIC_IDLE_TIMEOUT)
                    .unwrap_or_else(|error| error.into_inner());
                state = next;
                state.idle_workers = state.idle_workers.saturating_sub(1);
                if timeout.timed_out() && state.queue.is_empty() {
                    state.live_workers = state.live_workers.saturating_sub(1);
                    *active = false;
                    return WorkerTake::Exit;
                }
            }
        }
    }

    /// Removes an active worker and attempts replacement when open backlog needs one.
    fn worker_exited(self: &Arc<Self>) {
        let spawn_worker = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.live_workers = state.live_workers.saturating_sub(1);
            let spawn = self.reserve_elastic_locked(&mut state);
            self.ready.notify_all();
            spawn
        };
        if spawn_worker {
            self.spawn_elastic();
        }
    }

    /// Rejects new batches and wakes workers to drain the queue and exit.
    ///
    /// Workers process batches already queued before they observe closure.
    pub(super) fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.closed = true;
        self.ready.notify_all();
    }

    /// Records completion of one batch previously removed by [`take`](Self::take).
    fn finish_batch(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        debug_assert!(
            state.running_batches > 0,
            "a cleanup worker may finish only a batch it previously started"
        );
        state.running_batches = state.running_batches.saturating_sub(1);
    }

    /// Copies queued and running cleanup counts under the worker mutex.
    pub(super) fn snapshot(&self) -> CleanupSnapshot {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        CleanupSnapshot {
            queued: state.queue.len(),
            running: state.running_batches,
        }
    }

    /// Returns live, idle, and starting counts for focused tests.
    #[cfg(test)]
    pub(in crate::core::deferred_drop) fn worker_counts(&self) -> (usize, usize, usize) {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        (
            state.live_workers,
            state.idle_workers,
            state.starting_workers,
        )
    }
}

/// Queue ownership and readiness acknowledgement sent through a startup gate.
pub(in crate::core::deferred_drop) struct WorkerLaunch {
    /// Queue shared by this worker.
    queue: Arc<WorkerQueue>,
    /// Whether this worker belongs to the persistent core set.
    core: bool,
    /// Acknowledgement sent after a core worker becomes ready for work.
    started: Option<std::sync::mpsc::Sender<()>>,
}

/// Action returned by one worker queue turn.
enum WorkerTake {
    /// Run one batch and optionally start the next elastic worker.
    Batch {
        /// Charged destructor batch.
        batch: DropBatch,
        /// Whether backlog state reserved another elastic startup.
        spawn_elastic: bool,
    },
    /// Exit after the queue is closed or the elastic idle timeout expires.
    Exit,
}

/// Balances the running-batch count even if internal worker code unwinds.
struct RunningBatchGuard<'a> {
    /// Queue whose batch count was charged by `take`.
    queue: &'a WorkerQueue,
}

impl Drop for RunningBatchGuard<'_> {
    fn drop(&mut self) {
        self.queue.finish_batch();
    }
}

/// Creates named operating-system threads for the production domain.
pub(in crate::core::deferred_drop) fn system_spawner() -> Arc<WorkerSpawner> {
    Arc::new(|index, launcher| {
        std::thread::Builder::new()
            .name(format!("taskvisor-drop-{index}"))
            .spawn(move || {
                if let Ok(launch) = launcher.recv() {
                    worker_loop(launch);
                }
            })
    })
}

/// Runs one worker under an outer boundary for unexpected internal panics.
///
/// An unexpected panic payload is retained permanently before queue accounting replaces the worker when needed.
pub(in crate::core::deferred_drop) fn worker_loop(launch: WorkerLaunch) {
    let queue = Arc::clone(&launch.queue);
    let core = launch.core;
    let mut started = launch.started;
    let mut active = false;
    let result = catch_unwind(AssertUnwindSafe(|| {
        while let WorkerTake::Batch {
            batch,
            spawn_elastic,
        } = queue.take(core, &mut active, &mut started)
        {
            let running = RunningBatchGuard { queue: &queue };
            if spawn_elastic {
                queue.spawn_elastic();
            }
            batch.run();
            drop(running);
        }
    }));
    if let Err(payload) = result {
        std::mem::forget(payload);
    }
    if active {
        queue.worker_exited();
    }
}

/// Caps workers by both domain capacity and the global worker limit.
pub(super) fn max_worker_count(capacity: Option<NonZeroUsize>) -> usize {
    capacity.map_or(MAX_WORKER_COUNT, |capacity| {
        capacity.get().min(MAX_WORKER_COUNT)
    })
}
