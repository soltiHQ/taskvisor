//! Process-wide bounded isolation for synchronous task metadata.
//!
//! [`Task::name`](crate::Task::name) is user code: it may block or panic. Every
//! metadata job therefore runs on this fixed native worker set instead of a
//! Tokio runtime thread or the serialized controller loop. The job receives an
//! already charged [`OwnedTask`], so queued, running, canceled, and panicked
//! callbacks all remain covered by the global ownership budget.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex, OnceLock,
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
    },
};

use tokio::sync::oneshot;

use super::deferred_drop::{OWNERSHIP_CAPACITY, OwnedTask};

/// A small fixed set isolates blocked callbacks without creating one native
/// thread per task.
const WORKER_COUNT: usize = 2;

type MetadataJob = Box<dyn FnOnce() + Send + 'static>;

/// Typed result of one isolated `Task::name` callback.
pub(crate) enum TaskNameSnapshot<T> {
    Ready {
        owned: OwnedTask<T>,
        task_name: Arc<str>,
    },
    Panicked {
        owned: OwnedTask<T>,
        message: String,
    },
}

struct MetadataExecutor {
    sender: Option<SyncSender<MetadataJob>>,
}

impl MetadataExecutor {
    fn start() -> Self {
        let (sender, receiver) = sync_channel(OWNERSHIP_CAPACITY);
        let receiver = Arc::new(Mutex::new(receiver));
        let mut started = 0usize;
        for index in 0..WORKER_COUNT {
            let receiver = Arc::clone(&receiver);
            if std::thread::Builder::new()
                .name(format!("taskvisor-metadata-{index}"))
                .spawn(move || worker_loop(&receiver))
                .is_ok()
            {
                started += 1;
            }
        }
        Self {
            sender: (started != 0).then_some(sender),
        }
    }

    fn dispatch(&self, job: MetadataJob) -> Result<(), MetadataJob> {
        let Some(sender) = &self.sender else {
            return Err(job);
        };
        match sender.try_send(job) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(job) | TrySendError::Disconnected(job)) => Err(job),
        }
    }
}

fn worker_loop(receiver: &Mutex<Receiver<MetadataJob>>) {
    loop {
        let job = receiver
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .recv();
        let Ok(job) = job else {
            return;
        };
        job();
    }
}

fn executor() -> &'static MetadataExecutor {
    static EXECUTOR: OnceLock<MetadataExecutor> = OnceLock::new();
    EXECUTOR.get_or_init(MetadataExecutor::start)
}

/// Dispatches one already charged task and returns it intact if the fixed
/// executor cannot accept the job.
pub(crate) fn snapshot_task_name<T, F>(
    owned: OwnedTask<T>,
    snapshot: F,
) -> Result<oneshot::Receiver<TaskNameSnapshot<T>>, Box<OwnedTask<T>>>
where
    T: Send + 'static,
    F: FnOnce(&T) -> Arc<str> + Send + 'static,
{
    // The recovery cell keeps typed ownership available after a type-erased
    // channel rejection. The worker takes it before calling user code, and no
    // user destructor runs while this mutex is held.
    let ownership = Arc::new(Mutex::new(Some(owned)));
    let worker_ownership = Arc::clone(&ownership);
    let (reply, receiver) = oneshot::channel();
    let job: MetadataJob = Box::new(move || {
        let mut owned = worker_ownership
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .expect("one metadata job takes its charged task exactly once");
        let snapshot = match catch_unwind(AssertUnwindSafe(|| snapshot(&owned.value))) {
            Ok(task_name) => TaskNameSnapshot::Ready { owned, task_name },
            Err(payload) => {
                let message = payload
                    .downcast_ref::<&'static str>()
                    .map(|message| (*message).to_owned())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "non-string panic payload".to_owned());
                owned.cleanup.attach_panic_payload(payload);
                TaskNameSnapshot::Panicked { owned, message }
            }
        };
        // If the caller canceled its wait, dropping this typed result submits
        // the retained task to the bounded destructor executor.
        let _ = reply.send(snapshot);
    });

    if let Err(job) = executor().dispatch(job) {
        drop(job);
        let owned = ownership
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .expect("a rejected metadata job returns its charged task");
        return Err(Box::new(owned));
    }
    Ok(receiver)
}

#[cfg(test)]
pub(crate) async fn blocking_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    static GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    GUARD.lock().await
}
