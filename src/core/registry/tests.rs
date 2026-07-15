use super::*;
use crate::{
    RuntimeError, TaskOutcome, TaskSpec,
    core::actor::ActorExitReason,
    events::{Event, EventKind},
    reasons,
};
use std::{future::Future, pin::Pin, task::Poll};
use tokio::sync::oneshot;

async fn assert_pending_once<F: Future>(mut future: Pin<&mut F>) {
    std::future::poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("future completed before the expected ordering point"),
    })
    .await;
}

async fn assert_ready_once<F: Future<Output = ()>>(mut future: Pin<&mut F>) {
    std::future::poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Ready(()) => Poll::Ready(()),
        Poll::Pending => panic!("future was not immediately ready"),
    })
    .await;
}

#[tokio::test]
async fn pending_wait_drained_handles_empty_and_resolves_after_last_dec() {
    let p = Arc::new(PendingJoins::default());
    let mut initially_empty = Box::pin(p.wait_drained());
    assert_ready_once(initially_empty.as_mut()).await;
    drop(initially_empty);

    let a = TaskId::next();
    let b = TaskId::next();
    p.inc(a);
    p.inc(b);
    assert!(!p.is_empty());

    let mut drained = Box::pin(p.wait_drained());
    assert_pending_once(drained.as_mut()).await;
    p.dec(a);
    assert_pending_once(drained.as_mut()).await;
    p.dec(b);
    drained.await;
    assert!(p.is_empty(), "no joins should remain after draining");
}

fn registry() -> Arc<Registry> {
    let bus = Bus::new(64);
    let token = CancellationToken::new();
    let (_tx, rx) = mpsc::channel(64);
    Registry::new(
        bus,
        token,
        None,
        Duration::from_secs(5),
        TaskDefaults::default(),
        rx,
    )
}

#[tokio::test(flavor = "current_thread")]
async fn terminal_cleanup_wakes_all_empty_waiters() {
    use tokio::sync::Barrier;

    let registry = registry();
    let id = TaskId::next();
    let label: Arc<str> = Arc::from("empty-waiters");
    let completion = RemovalCompletion::new();
    let mut state = registry.state.write().await;
    state.by_label.insert(Arc::clone(&label), id);
    state.tasks.insert(
        id,
        Entry {
            label: Arc::clone(&label),
            state: EntryState::Removing {
                completion: completion.clone(),
            },
        },
    );
    registry.pending_joins.inc(id);
    registry.pending_joins.label(id, label);

    let ready = Arc::new(Barrier::new(3));
    let first_registry = Arc::clone(&registry);
    let first_ready = Arc::clone(&ready);
    let first = tokio::spawn(async move {
        first_ready.wait().await;
        first_registry.wait_until_empty().await;
    });
    let second_registry = Arc::clone(&registry);
    let second_ready = Arc::clone(&ready);
    let second = tokio::spawn(async move {
        second_ready.wait().await;
        second_registry.wait_until_empty().await;
    });

    ready.wait().await;
    tokio::task::yield_now().await;
    drop(state);

    let state_barrier = registry.state.write().await;
    drop(state_barrier);
    assert!(!first.is_finished());
    assert!(!second.is_finished());

    Registry::finish_removal(
        &registry.state,
        &registry.empty_notify,
        &registry.pending_joins,
        &registry.bus,
        RemovalReport {
            id,
            outcome: None,
            join: JoinCompletion::Joined(Ok(ActorExitReason::Completed)),
            completion,
        },
    )
    .await;

    tokio::time::timeout(Duration::from_secs(1), first)
        .await
        .expect("the first empty waiter must wake")
        .expect("the first empty waiter must not panic");
    tokio::time::timeout(Duration::from_secs(1), second)
        .await
        .expect("the second empty waiter must wake")
        .expect("the second empty waiter must not panic");
    assert!(registry.is_empty().await);
    assert_eq!(registry.id_for_label("empty-waiters").await, None);
    assert!(registry.pending_joins.is_empty());
}

fn started_registry(
    bus_capacity: usize,
    grace: Duration,
) -> (
    Arc<Registry>,
    Bus,
    CancellationToken,
    mpsc::Sender<RegistryCommand>,
) {
    let bus = Bus::new(bus_capacity);
    let token = CancellationToken::new();
    let (tx, rx) = mpsc::channel(64);
    let registry = Registry::new(
        bus.clone(),
        token.clone(),
        None,
        grace,
        TaskDefaults::default(),
        rx,
    );
    registry.clone().spawn_listener();
    (registry, bus, token, tx)
}

struct ControlledCancellationTask {
    task: crate::TaskRef,
    started: Arc<Notify>,
    cancellation_seen: Arc<Notify>,
    release: Arc<Notify>,
}

fn controlled_cancellation_task(label: &'static str) -> ControlledCancellationTask {
    let started = Arc::new(Notify::new());
    let cancellation_seen = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let started_by_task = Arc::clone(&started);
    let seen_by_task = Arc::clone(&cancellation_seen);
    let release_by_task = Arc::clone(&release);
    let task = crate::TaskFn::arc(label, move |ctx: crate::TaskContext| {
        let started = Arc::clone(&started_by_task);
        let cancellation_seen = Arc::clone(&seen_by_task);
        let release = Arc::clone(&release_by_task);
        async move {
            started.notify_one();
            ctx.cancelled().await;
            cancellation_seen.notify_one();
            release.notified().await;
            Err(crate::TaskError::Canceled)
        }
    });

    ControlledCancellationTask {
        task,
        started,
        cancellation_seen,
        release,
    }
}

fn send_add(
    tx: &mpsc::Sender<RegistryCommand>,
    id: TaskId,
    spec: TaskSpec,
    outcome: Option<OutcomeTx>,
) -> AddReplyRx {
    let (reply, reply_rx) = oneshot::channel();
    tx.try_send(RegistryCommand::Add {
        id,
        spec,
        outcome,
        completion: None,
        reply,
    })
    .expect("registry command channel must stay open");
    reply_rx
}

fn batch_item(id: TaskId, spec: TaskSpec) -> AddBatchItem {
    AddBatchItem {
        id,
        label: Arc::from(spec.task().name()),
        spec,
    }
}

fn send_batch(tx: &mpsc::Sender<RegistryCommand>, items: Vec<AddBatchItem>) -> AddReplyRx {
    let (reply, reply_rx) = oneshot::channel();
    tx.try_send(RegistryCommand::AddBatch { items, reply })
        .expect("registry command channel must stay open");
    reply_rx
}

fn send_remove(tx: &mpsc::Sender<RegistryCommand>, id: TaskId) -> RemoveReplyRx {
    let (reply, reply_rx) = oneshot::channel();
    tx.try_send(RegistryCommand::Remove { id, reply })
        .expect("registry command channel must stay open");
    reply_rx
}

fn send_cancel(tx: &mpsc::Sender<RegistryCommand>, id: TaskId) -> CancelReplyRx {
    let (reply, reply_rx) = oneshot::channel();
    tx.try_send(RegistryCommand::Cancel { id, reply })
        .expect("registry command channel must stay open");
    reply_rx
}

async fn receive_reply<T>(reply: oneshot::Receiver<T>, name: &str) -> T {
    tokio::time::timeout(Duration::from_secs(2), reply)
        .await
        .unwrap_or_else(|_| panic!("{name} timed out"))
        .unwrap_or_else(|_| panic!("{name} sender was dropped"))
}

async fn receive_completion(
    completion_rx: &mut mpsc::UnboundedReceiver<TaskId>,
    name: &str,
) -> TaskId {
    tokio::time::timeout(Duration::from_secs(2), completion_rx.recv())
        .await
        .unwrap_or_else(|_| panic!("{name} timed out"))
        .unwrap_or_else(|| panic!("{name} channel was closed"))
}

async fn stop_registry(registry: &Registry, token: &CancellationToken) {
    token.cancel();
    tokio::time::timeout(Duration::from_secs(2), registry.join_listener())
        .await
        .expect("registry listener must stop");
}

#[tokio::test(flavor = "current_thread")]
async fn add_reply_commits_state_without_event_confirmation() {
    use crate::{TaskContext, TaskFn, TaskRef};
    use tokio::sync::broadcast::error::TryRecvError;

    let (registry, bus, token, tx) = started_registry(1, Duration::from_secs(1));
    let mut stale_events = bus.subscribe();
    let id = TaskId::next();
    let task: TaskRef = TaskFn::arc("reply-add", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });

    let reply = send_add(&tx, id, TaskSpec::restartable(task), None);
    assert!(
        receive_reply(reply, "add reply").await.is_ok(),
        "registry must accept a unique task"
    );
    assert!(
        registry.contains(id).await,
        "reply requires committed id state"
    );
    assert_eq!(
        registry.id_for_label("reply-add").await,
        Some(id),
        "reply requires committed label state"
    );
    assert_eq!(registry.list().await, vec![(id, Arc::from("reply-add"))]);

    for _ in 0..4 {
        bus.publish(Event::new(EventKind::TaskStarting).with_task("noise"));
    }
    assert!(
        matches!(stale_events.try_recv(), Err(TryRecvError::Lagged(_))),
        "the observer must lag in this regression setup"
    );
    assert!(
        registry.contains(id).await,
        "event lag must not change the authoritative add result"
    );

    stop_registry(&registry, &token).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_add_publishes_added_before_starting() {
    use crate::{TaskFn, TaskRef};

    const TASKS: usize = 4_096;

    let bus = Bus::new(TASKS * 8);
    let mut events = bus.subscribe();
    let token = CancellationToken::new();
    let (_tx, rx) = mpsc::channel(1);
    let registry = Registry::new(
        bus,
        token.clone(),
        None,
        Duration::from_secs(1),
        TaskDefaults::default(),
        rx,
    );
    registry.clone().spawn_listener();

    let mut registrations = tokio::task::JoinSet::new();
    for index in 0..TASKS {
        let registry = Arc::clone(&registry);
        registrations.spawn(async move {
            let id = TaskId::next();
            let task: TaskRef =
                TaskFn::arc(format!("ordered-add-{index}"), |_ctx| async { Ok(()) });
            let (reply, reply_rx) = oneshot::channel();
            registry
                .spawn_and_register(id, TaskSpec::once(task), None, None, reply)
                .await;
            assert!(
                matches!(reply_rx.await, Ok(Ok(()))),
                "single registration must succeed"
            );
            id
        });
    }

    while let Some(result) = registrations.join_next().await {
        result.expect("registration worker must not panic");
    }
    tokio::time::timeout(Duration::from_secs(5), registry.wait_until_empty())
        .await
        .expect("all one-shot actors must be reaped");

    let mut order = std::collections::HashMap::<TaskId, (Option<usize>, Option<usize>)>::new();
    for position in 0.. {
        let Ok(event) = events.try_recv() else {
            break;
        };
        let Some(id) = event.id else {
            continue;
        };
        let entry = order.entry(id).or_default();
        match event.kind {
            EventKind::TaskAdded => entry.0 = Some(position),
            EventKind::TaskStarting => {
                entry.1.get_or_insert(position);
            }
            _ => {}
        }
    }

    assert_eq!(order.len(), TASKS, "every registration must be observed");
    for (id, (added, starting)) in order {
        let added = added.unwrap_or_else(|| panic!("{id} is missing TaskAdded"));
        let starting = starting.unwrap_or_else(|| panic!("{id} is missing TaskStarting"));
        assert!(
            added < starting,
            "{id} delivered TaskStarting before TaskAdded: added={added}, starting={starting}"
        );
    }

    stop_registry(&registry, &token).await;
}

#[tokio::test(flavor = "current_thread")]
async fn batch_reply_commits_every_task_as_one_registry_decision() {
    use crate::{TaskContext, TaskFn, TaskRef};

    let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(1));
    let mut events = bus.subscribe();
    let mut expected = Vec::new();
    let mut items = Vec::new();
    for label in ["batch-a", "batch-b", "batch-c"] {
        let id = TaskId::next();
        let task: TaskRef = TaskFn::arc(label, |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        expected.push((id, Arc::from(label)));
        items.push(batch_item(id, TaskSpec::restartable(task)));
    }
    expected.sort_by_key(|(id, _)| *id);

    let result = receive_reply(send_batch(&tx, items), "batch add reply").await;
    assert!(result.is_ok(), "unique batch must be accepted: {result:?}");
    assert_eq!(registry.list().await, expected);

    let added: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
        .filter(|event| event.kind == EventKind::TaskAdded)
        .collect();
    assert_eq!(added.len(), 3);

    stop_registry(&registry, &token).await;
}

#[tokio::test(flavor = "current_thread")]
async fn dropped_batch_reply_still_starts_after_all_added_events() {
    use crate::{TaskContext, TaskFn, TaskRef};

    let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(1));
    let mut events = bus.subscribe();
    let (body_tx, mut body_rx) = mpsc::unbounded_channel();
    let mut items = Vec::new();
    for label in ["dropped-batch-a", "dropped-batch-b"] {
        let id = TaskId::next();
        let body_tx = body_tx.clone();
        let task: TaskRef = TaskFn::arc(label, move |_ctx: TaskContext| {
            let _ = body_tx.send(id);
            async { Ok(()) }
        });
        items.push(batch_item(id, TaskSpec::once(task)));
    }
    drop(body_tx);

    let reply = send_batch(&tx, items);
    drop(reply);
    let first = receive_completion(&mut body_rx, "first batch body").await;
    let second = receive_completion(&mut body_rx, "second batch body").await;
    assert_ne!(first, second);
    tokio::time::timeout(Duration::from_secs(2), registry.wait_until_empty())
        .await
        .expect("both one-shot batch tasks must finish");

    let observed: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
    let added: Vec<_> = observed
        .iter()
        .filter(|event| event.kind == EventKind::TaskAdded)
        .collect();
    let starting: Vec<_> = observed
        .iter()
        .filter(|event| event.kind == EventKind::TaskStarting)
        .collect();
    assert_eq!(added.len(), 2);
    assert_eq!(starting.len(), 2);
    let last_added = added.iter().map(|event| event.seq).max().unwrap();
    let first_starting = starting.iter().map(|event| event.seq).min().unwrap();
    assert!(
        last_added < first_starting,
        "the batch start gate must keep bodies behind all TaskAdded events"
    );

    stop_registry(&registry, &token).await;
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_inside_batch_rejects_every_item_without_starting_bodies() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::{TaskContext, TaskFn, TaskRef};

    let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(1));
    let mut events = bus.subscribe();
    let runs = Arc::new(AtomicUsize::new(0));
    let mut items = Vec::new();
    let mut ids = Vec::new();
    for label in ["unique", "duplicate", "duplicate"] {
        let runs = Arc::clone(&runs);
        let task: TaskRef = TaskFn::arc(label, move |_ctx: TaskContext| {
            runs.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        });
        let id = TaskId::next();
        ids.push(id);
        items.push(batch_item(id, TaskSpec::once(task)));
    }

    let result = receive_reply(send_batch(&tx, items), "duplicate batch reply").await;
    assert!(
        matches!(
            result,
            Err(RuntimeError::TaskAlreadyExists { ref name }) if name.as_ref() == "duplicate"
        ),
        "the first conflicting input label must reject the batch: {result:?}"
    );
    assert!(registry.list().await.is_empty());
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    let observed: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
    assert_eq!(
        observed
            .iter()
            .filter(|event| event.kind == EventKind::TaskAdded)
            .count(),
        0
    );
    let failed: Vec<_> = observed
        .into_iter()
        .filter(|event| event.kind == EventKind::TaskAddFailed)
        .collect();
    assert_eq!(failed.len(), 3);
    assert_eq!(failed[0].id, Some(ids[0]));
    assert_eq!(failed[0].reason.as_deref(), Some(reasons::BATCH_REJECTED));
    assert_eq!(failed[1].id, Some(ids[1]));
    assert_eq!(failed[1].reason.as_deref(), Some(reasons::BATCH_REJECTED));
    assert_eq!(failed[2].id, Some(ids[2]));
    assert_eq!(failed[2].reason.as_deref(), Some(reasons::ALREADY_EXISTS));

    stop_registry(&registry, &token).await;
}

#[tokio::test(flavor = "current_thread")]
async fn batch_conflict_with_registered_or_removing_label_starts_no_new_body() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::{TaskContext, TaskFn, TaskRef};

    let (registry, _bus, token, tx) = started_registry(64, Duration::from_secs(1));
    let controlled = controlled_cancellation_task("reserved-batch-name");
    let existing_id = TaskId::next();
    assert!(
        receive_reply(
            send_add(
                &tx,
                existing_id,
                TaskSpec::restartable(controlled.task),
                None,
            ),
            "existing add reply",
        )
        .await
        .is_ok()
    );
    tokio::time::timeout(Duration::from_secs(2), controlled.started.notified())
        .await
        .expect("the existing task body must start before removal");

    let candidate_runs = Arc::new(AtomicUsize::new(0));
    let make_candidate = |label: &'static str| {
        let runs = Arc::clone(&candidate_runs);
        let task: TaskRef = TaskFn::arc(label, move |_ctx: TaskContext| {
            runs.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        });
        batch_item(TaskId::next(), TaskSpec::once(task))
    };

    let registered_result = receive_reply(
        send_batch(
            &tx,
            vec![
                make_candidate("registered-peer"),
                make_candidate("reserved-batch-name"),
            ],
        ),
        "registered conflict batch",
    )
    .await;
    assert!(matches!(
        registered_result,
        Err(RuntimeError::TaskAlreadyExists { name })
            if name.as_ref() == "reserved-batch-name"
    ));
    assert_eq!(candidate_runs.load(Ordering::SeqCst), 0);
    assert_eq!(
        registry.list().await,
        vec![(existing_id, Arc::from("reserved-batch-name"))]
    );

    assert!(matches!(
        receive_reply(send_remove(&tx, existing_id), "existing remove reply").await,
        Ok(true)
    ));
    tokio::time::timeout(
        Duration::from_secs(2),
        controlled.cancellation_seen.notified(),
    )
    .await
    .expect("the existing task must enter Removing");

    let removing_result = receive_reply(
        send_batch(
            &tx,
            vec![
                make_candidate("removing-peer"),
                make_candidate("reserved-batch-name"),
            ],
        ),
        "removing conflict batch",
    )
    .await;
    assert!(matches!(
        removing_result,
        Err(RuntimeError::TaskAlreadyExists { name })
            if name.as_ref() == "reserved-batch-name"
    ));
    assert_eq!(candidate_runs.load(Ordering::SeqCst), 0);
    assert_eq!(
        registry.list().await,
        vec![(existing_id, Arc::from("reserved-batch-name"))]
    );

    controlled.release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), registry.wait_until_empty())
        .await
        .expect("the existing removing task must finish");
    stop_registry(&registry, &token).await;
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_add_reply_rejects_without_starting_body() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::{TaskContext, TaskFn, TaskRef};

    let (registry, _bus, token, tx) = started_registry(64, Duration::from_secs(1));
    let first_id = TaskId::next();
    let first: TaskRef = TaskFn::arc("duplicate", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    assert!(
        receive_reply(
            send_add(&tx, first_id, TaskSpec::restartable(first), None),
            "first add reply",
        )
        .await
        .is_ok()
    );

    let runs = Arc::new(AtomicUsize::new(0));
    let duplicate_runs = Arc::clone(&runs);
    let duplicate: TaskRef = TaskFn::arc("duplicate", move |_ctx: TaskContext| {
        duplicate_runs.fetch_add(1, Ordering::SeqCst);
        async { Ok(()) }
    });
    let second_id = TaskId::next();
    let (outcome, outcome_rx) = oneshot::channel();
    let duplicate_reply = receive_reply(
        send_add(&tx, second_id, TaskSpec::once(duplicate), Some(outcome)),
        "duplicate add reply",
    )
    .await;

    assert!(
        matches!(
            duplicate_reply,
            Err(RuntimeError::TaskAlreadyExists { name }) if name.as_ref() == "duplicate"
        ),
        "duplicate add must return its authoritative rejection"
    );
    assert!(!registry.contains(second_id).await);
    assert_eq!(registry.id_for_label("duplicate").await, Some(first_id));
    assert_eq!(runs.load(Ordering::SeqCst), 0, "rejected body must not run");
    assert!(matches!(
        receive_reply(outcome_rx, "duplicate outcome").await,
        TaskOutcome::Rejected { reason, .. } if reason.as_ref() == reasons::ALREADY_EXISTS
    ));

    stop_registry(&registry, &token).await;
}

#[tokio::test(flavor = "current_thread")]
async fn remove_reply_claims_once_before_terminal_completion() {
    let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(1));
    let mut events = bus.subscribe();
    let controlled = controlled_cancellation_task("remove-once");
    let id = TaskId::next();
    assert!(
        receive_reply(
            send_add(&tx, id, TaskSpec::restartable(controlled.task), None),
            "setup add reply",
        )
        .await
        .is_ok()
    );
    while events.try_recv().is_ok() {}

    assert!(
        matches!(
            receive_reply(send_remove(&tx, id), "first remove reply").await,
            Ok(true)
        ),
        "the first remove must claim the task"
    );
    tokio::time::timeout(
        Duration::from_secs(2),
        controlled.cancellation_seen.notified(),
    )
    .await
    .expect("the task must observe cancellation");
    assert!(registry.pending_joins.contains(id));
    assert!(
        registry.contains(id).await,
        "a removing task must keep its registry identity"
    );
    assert_eq!(
        registry.list().await,
        vec![(id, Arc::from("remove-once"))],
        "a removing task must stay visible in registry listings"
    );
    assert_eq!(registry.id_for_label("remove-once").await, Some(id));
    let mut empty = Box::pin(registry.wait_until_empty());
    assert_pending_once(empty.as_mut()).await;
    drop(empty);
    while let Ok(event) = events.try_recv() {
        assert_ne!(
            event.kind,
            EventKind::TaskRemoved,
            "remove reply must not wait for or invent terminal completion"
        );
    }

    assert!(
        matches!(
            receive_reply(send_remove(&tx, id), "second remove reply").await,
            Ok(false)
        ),
        "a second remove cannot claim the same task"
    );

    let joined_cancel = receive_reply(send_cancel(&tx, id), "joined cancel reply")
        .await
        .expect("the cancel command must succeed")
        .expect("the removing task must expose its completion");
    assert!(
        !joined_cancel.claimed,
        "cancel must join an existing Remove instead of claiming again"
    );
    assert!(
        !joined_cancel.is_complete(),
        "joining cancellation cannot complete before the actor join"
    );

    controlled.release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), joined_cancel.wait())
        .await
        .expect("joined cancellation must finish with the Remove owner");
    tokio::time::timeout(
        Duration::from_secs(2),
        registry.pending_joins.wait_drained(),
    )
    .await
    .expect("the released task must finish its join");
    assert!(!registry.contains(id).await);
    assert_eq!(registry.id_for_label("remove-once").await, None);
    stop_registry(&registry, &token).await;
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_cancel_commands_share_one_terminal_completion() {
    let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(5));
    let mut events = bus.subscribe();
    let controlled = controlled_cancellation_task("shared-cancel");
    let id = TaskId::next();
    assert!(
        receive_reply(
            send_add(&tx, id, TaskSpec::restartable(controlled.task), None),
            "shared cancel add reply",
        )
        .await
        .is_ok()
    );
    while events.try_recv().is_ok() {}

    const CALLERS: usize = 8;
    let replies: Vec<_> = (0..CALLERS).map(|_| send_cancel(&tx, id)).collect();
    let mut decisions = Vec::with_capacity(CALLERS);
    for reply in replies {
        decisions.push(
            receive_reply(reply, "concurrent cancel reply")
                .await
                .expect("cancel command must succeed")
                .expect("the task must still be removing"),
        );
    }
    tokio::time::timeout(
        Duration::from_secs(2),
        controlled.cancellation_seen.notified(),
    )
    .await
    .expect("the task must observe one cancellation");

    assert_eq!(
        decisions.iter().filter(|decision| decision.claimed).count(),
        1,
        "exactly one cancellation command may claim the task"
    );
    assert!(decisions.iter().all(|decision| !decision.is_complete()));
    assert!(registry.contains(id).await);
    assert!(
        std::iter::from_fn(|| events.try_recv().ok())
            .all(|event| event.id != Some(id) || event.kind != EventKind::TaskRemoved),
        "terminal cleanup cannot happen before the task is released"
    );

    controlled.release.notify_one();
    for decision in &decisions {
        tokio::time::timeout(Duration::from_secs(2), decision.wait())
            .await
            .expect("all cancel callers must share terminal completion");
    }
    tokio::time::timeout(Duration::from_secs(2), registry.wait_until_empty())
        .await
        .expect("terminal cleanup must remove the task");
    let removed = std::iter::from_fn(|| events.try_recv().ok())
        .filter(|event| event.id == Some(id) && event.kind == EventKind::TaskRemoved)
        .count();
    assert_eq!(
        removed, 1,
        "shared cancellation must publish one terminal event"
    );

    stop_registry(&registry, &token).await;
}

#[tokio::test(flavor = "current_thread")]
async fn removing_task_keeps_label_reserved_until_terminal_join() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::{TaskContext, TaskFn, TaskRef};

    let (registry, _bus, token, tx) = started_registry(64, Duration::from_secs(5));
    let controlled = controlled_cancellation_task("reserved-name");
    let first_id = TaskId::next();
    assert!(
        receive_reply(
            send_add(&tx, first_id, TaskSpec::restartable(controlled.task), None,),
            "reserved-name add reply",
        )
        .await
        .is_ok()
    );
    assert!(matches!(
        receive_reply(send_remove(&tx, first_id), "reserved-name remove reply").await,
        Ok(true)
    ));
    tokio::time::timeout(
        Duration::from_secs(2),
        controlled.cancellation_seen.notified(),
    )
    .await
    .expect("the old task must observe cancellation");

    let duplicate_runs = Arc::new(AtomicUsize::new(0));
    let runs_by_task = Arc::clone(&duplicate_runs);
    let duplicate: TaskRef = TaskFn::arc("reserved-name", move |_ctx: TaskContext| {
        runs_by_task.fetch_add(1, Ordering::SeqCst);
        async { Ok(()) }
    });
    let duplicate_id = TaskId::next();
    let duplicate_reply = receive_reply(
        send_add(&tx, duplicate_id, TaskSpec::once(duplicate), None),
        "removing duplicate add reply",
    )
    .await;
    assert!(
        matches!(
            duplicate_reply,
            Err(RuntimeError::TaskAlreadyExists { name })
                if name.as_ref() == "reserved-name"
        ),
        "a removing task must keep its label reserved"
    );
    assert_eq!(
        duplicate_runs.load(Ordering::SeqCst),
        0,
        "a rejected replacement body must not run"
    );
    assert_eq!(registry.id_for_label("reserved-name").await, Some(first_id));
    assert_eq!(
        registry.list().await,
        vec![(first_id, Arc::from("reserved-name"))]
    );
    let mut empty = Box::pin(registry.wait_until_empty());
    assert_pending_once(empty.as_mut()).await;
    drop(empty);

    controlled.release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), registry.wait_until_empty())
        .await
        .expect("terminal join must release the old task identity");
    assert_eq!(registry.id_for_label("reserved-name").await, None);
    assert!(!registry.pending_joins.contains(first_id));

    let replacement: TaskRef = TaskFn::arc("reserved-name", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    let replacement_id = TaskId::next();
    assert!(
        receive_reply(
            send_add(
                &tx,
                replacement_id,
                TaskSpec::restartable(replacement),
                None,
            ),
            "replacement add reply",
        )
        .await
        .is_ok(),
        "the label must be reusable after terminal cleanup"
    );
    assert_eq!(
        registry.id_for_label("reserved-name").await,
        Some(replacement_id)
    );

    stop_registry(&registry, &token).await;
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_remove_replies_false_without_pending_join() {
    let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(1));
    let mut events = bus.subscribe();
    let unknown = TaskId::next();

    assert!(
        matches!(
            receive_reply(send_remove(&tx, unknown), "unknown remove reply").await,
            Ok(false)
        ),
        "unknown remove must return false"
    );
    assert!(matches!(
        receive_reply(
            send_remove(&tx, TaskId::next()),
            "unknown remove barrier reply",
        )
        .await,
        Ok(false)
    ));
    assert!(
        registry.pending_joins.is_empty(),
        "unknown removal must not leak pending join state"
    );
    assert!(
        std::iter::from_fn(|| events.try_recv().ok())
            .all(|event| event.id != Some(unknown) || event.kind != EventKind::TaskRemoved),
        "unknown removal must not invent a terminal event"
    );

    stop_registry(&registry, &token).await;
}

#[tokio::test(flavor = "current_thread")]
async fn dropped_add_reply_does_not_stop_command_processing() {
    use crate::{TaskContext, TaskFn, TaskRef};

    let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(1));
    let mut events = bus.subscribe();
    let first_id = TaskId::next();
    let first: TaskRef = TaskFn::arc("dropped-add-a", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    drop(send_add(&tx, first_id, TaskSpec::restartable(first), None));

    let second_id = TaskId::next();
    let second: TaskRef = TaskFn::arc("dropped-add-b", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    assert!(
        receive_reply(
            send_add(&tx, second_id, TaskSpec::restartable(second), None),
            "second add reply",
        )
        .await
        .is_ok()
    );

    assert!(registry.contains(first_id).await);
    assert!(registry.contains(second_id).await);
    let mut added = 0;
    while let Ok(event) = events.try_recv() {
        if event.kind == EventKind::TaskAdded {
            added += 1;
        }
    }
    assert_eq!(added, 2, "a dropped reply must not suppress TaskAdded");

    stop_registry(&registry, &token).await;
}

#[tokio::test(flavor = "current_thread")]
async fn dropped_remove_reply_does_not_skip_join_cleanup() {
    let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(1));
    let mut events = bus.subscribe();
    let controlled = controlled_cancellation_task("dropped-remove");
    let id = TaskId::next();
    assert!(
        receive_reply(
            send_add(&tx, id, TaskSpec::restartable(controlled.task), None),
            "setup add reply",
        )
        .await
        .is_ok()
    );
    while events.try_recv().is_ok() {}

    drop(send_remove(&tx, id));
    assert!(
        matches!(
            receive_reply(
                send_remove(&tx, TaskId::next()),
                "synchronizing remove reply",
            )
            .await,
            Ok(false)
        ),
        "the listener must process commands after a dropped reply"
    );
    tokio::time::timeout(
        Duration::from_secs(2),
        controlled.cancellation_seen.notified(),
    )
    .await
    .expect("dropped receiver must not suppress cancellation");

    controlled.release.notify_one();
    tokio::time::timeout(
        Duration::from_secs(2),
        registry.pending_joins.wait_drained(),
    )
    .await
    .expect("dropped receiver must not suppress join cleanup");
    let mut saw_removed = false;
    while let Ok(event) = events.try_recv() {
        if event.kind == EventKind::TaskRemoved && event.id == Some(id) {
            saw_removed = true;
        }
    }
    assert!(saw_removed, "join cleanup must still publish TaskRemoved");

    stop_registry(&registry, &token).await;
}

#[tokio::test(start_paused = true)]
async fn wait_joins_within_reports_stuck_labels_then_drains() {
    let reg = registry();

    assert!(
        reg.wait_joins_within(Duration::from_millis(50))
            .await
            .is_empty(),
        "an empty join set must drain immediately"
    );

    let id = TaskId::next();
    reg.pending_joins.inc(id);
    reg.pending_joins.label(id, Arc::from("stuck-task"));
    let stuck = reg.wait_joins_within(Duration::from_millis(30)).await;
    assert_eq!(
        stuck,
        vec![Arc::<str>::from("stuck-task")],
        "an in-flight join must be reported with its label on timeout"
    );

    let mut draining = Box::pin(reg.wait_joins_within(Duration::from_secs(1)));
    assert_pending_once(draining.as_mut()).await;
    reg.pending_joins.dec(id);
    assert!(
        draining.await.is_empty(),
        "must drain once the in-flight join is decremented"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn completion_guard_signals_on_panic_and_abort_before_first_poll() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();

    let panic_id = TaskId::next();
    let panic_handle = Registry::spawn_tracked_actor(panic_id, completion_tx.clone(), async move {
        panic!("outer actor panic")
    });
    let panic_result = panic_handle.await;
    assert!(
        panic_result.is_err_and(|error| error.is_panic()),
        "outer actor panic must stay visible through JoinError"
    );
    assert_eq!(
        receive_completion(&mut completion_rx, "panic completion").await,
        panic_id
    );

    let polled = Arc::new(AtomicBool::new(false));
    let polled_by_task = Arc::clone(&polled);
    let abort_id = TaskId::next();
    let abort_handle = Registry::spawn_tracked_actor(abort_id, completion_tx, async move {
        polled_by_task.store(true, Ordering::SeqCst);
        std::future::pending::<()>().await;
        ActorExitReason::Completed
    });
    abort_handle.abort();
    let abort_result = abort_handle.await;
    assert!(
        abort_result.is_err_and(|error| error.is_cancelled()),
        "aborted actor must return a cancelled JoinError"
    );
    assert!(
        !polled.load(Ordering::SeqCst),
        "the abort regression requires abort-before-first-poll"
    );
    assert_eq!(
        receive_completion(&mut completion_rx, "abort completion").await,
        abort_id
    );
    assert!(
        completion_rx.try_recv().is_err(),
        "each actor exit must send one completion identity"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn natural_completion_cleans_registry_when_event_observer_lags() {
    use crate::{TaskContext, TaskFn, TaskRef};
    use tokio::sync::broadcast::error::TryRecvError;

    let (registry, bus, token, tx) = started_registry(1, Duration::from_secs(1));
    let mut stale_events = bus.subscribe();
    let task: TaskRef = TaskFn::arc("completion-no-bus", |_ctx: TaskContext| async { Ok(()) });
    let id = TaskId::next();
    let (outcome, outcome_rx) = oneshot::channel();

    assert!(
        receive_reply(
            send_add(&tx, id, TaskSpec::once(task), Some(outcome)),
            "fast add reply",
        )
        .await
        .is_ok()
    );
    tokio::time::timeout(Duration::from_secs(2), registry.wait_until_empty())
        .await
        .expect("completion channel must remove the finished task");
    assert!(
        registry
            .wait_joins_within(Duration::from_secs(2))
            .await
            .is_empty()
    );
    assert!(matches!(
        receive_reply(outcome_rx, "fast task outcome").await,
        TaskOutcome::Completed
    ));
    assert_eq!(registry.id_for_label("completion-no-bus").await, None);
    assert!(
        matches!(stale_events.try_recv(), Err(TryRecvError::Lagged(_))),
        "the observer must lose terminal events in this regression setup"
    );

    stop_registry(&registry, &token).await;
}

#[tokio::test(flavor = "current_thread")]
async fn forged_terminal_event_does_not_remove_running_actor() {
    use crate::{TaskContext, TaskFn, TaskRef};

    let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(1));
    let _observer = bus.subscribe();
    assert_eq!(
        bus.receiver_count(),
        1,
        "the registry listener must not subscribe to the event bus"
    );
    let id = TaskId::next();
    let task: TaskRef = TaskFn::arc("ignore-terminal-event", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    assert!(
        receive_reply(
            send_add(&tx, id, TaskSpec::restartable(task), None),
            "running add reply",
        )
        .await
        .is_ok()
    );

    bus.publish(
        Event::new(EventKind::ActorExhausted)
            .with_task("ignore-terminal-event")
            .with_id(id),
    );

    let barrier_id = TaskId::next();
    let barrier: TaskRef = TaskFn::arc("event-barrier", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    assert!(
        receive_reply(
            send_add(&tx, barrier_id, TaskSpec::restartable(barrier), None,),
            "barrier add reply",
        )
        .await
        .is_ok()
    );
    assert!(
        registry.contains(id).await,
        "terminal events are observability and cannot trigger cleanup"
    );

    stop_registry(&registry, &token).await;
}

#[tokio::test(flavor = "current_thread")]
async fn outer_actor_panic_is_reaped_by_completion_channel() {
    let (registry, bus, token, _tx) = started_registry(64, Duration::from_secs(1));
    let mut events = bus.subscribe();
    let id = TaskId::next();
    let label: Arc<str> = Arc::from("outer-panic");
    let (done, done_rx) = oneshot::channel();

    let mut state = registry.state.write().await;
    let join =
        Registry::spawn_tracked_actor(id, registry.listener.completion_tx.clone(), async move {
            panic!("outer actor panic")
        });
    state.by_label.insert(Arc::clone(&label), id);
    state.tasks.insert(
        id,
        Entry {
            label: Arc::clone(&label),
            state: EntryState::Registered(Handle {
                join,
                cancel: CancellationToken::new(),
                done: Some(done),
                completion: RemovalCompletion::new(),
            }),
        },
    );
    drop(state);

    assert!(matches!(
        receive_reply(done_rx, "panic outcome").await,
        TaskOutcome::Panicked
    ));
    tokio::time::timeout(Duration::from_secs(2), registry.wait_until_empty())
        .await
        .expect("panicked actor must leave the registry");
    assert!(
        registry
            .wait_joins_within(Duration::from_secs(2))
            .await
            .is_empty()
    );

    let mut actor_dead = 0;
    let mut task_removed = 0;
    while let Ok(event) = events.try_recv() {
        if event.id == Some(id) && event.kind == EventKind::ActorDead {
            actor_dead += 1;
        }
        if event.id == Some(id) && event.kind == EventKind::TaskRemoved {
            task_removed += 1;
        }
    }
    assert_eq!(actor_dead, 1);
    assert_eq!(task_removed, 1);

    stop_registry(&registry, &token).await;
}

#[tokio::test(flavor = "current_thread")]
async fn remove_path_owns_cleanup_when_completion_signal_arrives() {
    let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(1));
    let mut events = bus.subscribe();
    let controlled = controlled_cancellation_task("remove-completion-race");
    let id = TaskId::next();
    let (done, done_rx) = oneshot::channel();
    assert!(
        receive_reply(
            send_add(&tx, id, TaskSpec::restartable(controlled.task), Some(done),),
            "race add reply",
        )
        .await
        .is_ok()
    );
    while events.try_recv().is_ok() {}

    assert!(matches!(
        receive_reply(send_remove(&tx, id), "race remove reply").await,
        Ok(true)
    ));
    tokio::time::timeout(
        Duration::from_secs(2),
        controlled.cancellation_seen.notified(),
    )
    .await
    .expect("removed task must observe cancellation");
    controlled.release.notify_one();
    assert!(matches!(
        receive_reply(done_rx, "race outcome").await,
        TaskOutcome::Canceled
    ));
    tokio::time::timeout(
        Duration::from_secs(2),
        registry.pending_joins.wait_drained(),
    )
    .await
    .expect("remove-owned join must drain");

    assert!(matches!(
        receive_reply(send_remove(&tx, TaskId::next()), "completion barrier reply",).await,
        Ok(false)
    ));
    let removed_count = std::iter::from_fn(|| events.try_recv().ok())
        .filter(|event| event.id == Some(id) && event.kind == EventKind::TaskRemoved)
        .count();
    assert_eq!(
        removed_count, 1,
        "stale completion signal must not duplicate terminal cleanup"
    );

    stop_registry(&registry, &token).await;
}

#[tokio::test(flavor = "current_thread")]
async fn completion_claim_before_remove_emits_one_terminal_event() {
    use crate::{TaskContext, TaskFn, TaskRef};

    let (registry, bus, token, tx) = started_registry(64, Duration::from_secs(5));
    let mut events = bus.subscribe();
    let release = Arc::new(Notify::new());
    let task_release = Arc::clone(&release);
    let task: TaskRef = TaskFn::arc("completion-first", move |_ctx: TaskContext| {
        let release = Arc::clone(&task_release);
        async move {
            release.notified().await;
            Ok(())
        }
    });
    let id = TaskId::next();
    let (done, mut done_rx) = oneshot::channel();
    assert!(
        receive_reply(
            send_add(&tx, id, TaskSpec::once(task), Some(done)),
            "completion-first add reply",
        )
        .await
        .is_ok()
    );
    while events.try_recv().is_ok() {}

    registry
        .listener
        .completion_tx
        .send(id)
        .expect("completion receiver must be open");
    assert!(matches!(
        receive_reply(
            send_remove(&tx, TaskId::next()),
            "completion claim barrier reply",
        )
        .await,
        Ok(false)
    ));
    assert!(registry.pending_joins.contains(id));
    assert!(registry.contains(id).await);
    assert_eq!(registry.id_for_label("completion-first").await, Some(id));
    let mut empty = Box::pin(registry.wait_until_empty());
    assert_pending_once(empty.as_mut()).await;
    drop(empty);
    assert!(matches!(
        receive_reply(send_remove(&tx, id), "completion-first remove reply").await,
        Ok(false)
    ));
    let joined_cancel = receive_reply(send_cancel(&tx, id), "completion-first cancel reply")
        .await
        .expect("the cancel command must succeed")
        .expect("the completion-owned removal must still exist");
    assert!(
        !joined_cancel.claimed,
        "cancel must join the completion-plane owner"
    );
    assert!(!joined_cancel.is_complete());
    assert!(
        std::iter::from_fn(|| events.try_recv().ok())
            .all(|event| event.id != Some(id) || event.kind != EventKind::TaskRemoved),
        "remove must not report termination while cleanup is still joining"
    );

    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), joined_cancel.wait())
        .await
        .expect("cancel must finish with the completion-plane owner");
    assert!(
        matches!(done_rx.try_recv(), Ok(TaskOutcome::Completed)),
        "watched outcome must be ready before terminal completion is signalled"
    );
    tokio::time::timeout(Duration::from_secs(2), registry.wait_until_empty())
        .await
        .expect("completion-owned join must finish registry cleanup");
    assert!(!registry.pending_joins.contains(id));

    assert!(matches!(
        receive_reply(
            send_remove(&tx, TaskId::next()),
            "duplicate completion barrier reply",
        )
        .await,
        Ok(false)
    ));
    let removed_count = std::iter::from_fn(|| events.try_recv().ok())
        .filter(|event| event.id == Some(id) && event.kind == EventKind::TaskRemoved)
        .count();
    assert_eq!(
        removed_count, 1,
        "completion-first race must publish one terminal event"
    );

    stop_registry(&registry, &token).await;
}

#[tokio::test]
async fn shutdown_drains_buffered_command_and_never_silently_drops() {
    use crate::{TaskContext, TaskError, TaskFn, TaskRef};

    let bus = Bus::new(64);
    let token = CancellationToken::new();
    let (tx, rx) = mpsc::channel(1);
    let reg = Registry::new(
        bus,
        token.clone(),
        None,
        Duration::from_millis(50),
        TaskDefaults::default(),
        rx,
    );

    let task: TaskRef = TaskFn::arc("buffered", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Err(TaskError::Canceled)
    });
    let (done_tx, done_rx) = oneshot::channel();
    let (reply_tx, reply_rx) = oneshot::channel();
    let id = TaskId::next();
    tx.try_send(RegistryCommand::Add {
        id,
        spec: TaskSpec::restartable(task),
        outcome: Some(done_tx),
        completion: None,
        reply: reply_tx,
    })
    .expect("channel is open before shutdown");

    token.cancel();
    reg.clone().spawn_listener();
    tokio::time::timeout(Duration::from_secs(2), reg.join_listener())
        .await
        .expect("join_listener must not hang");

    let reply = tokio::time::timeout(Duration::from_secs(1), reply_rx)
        .await
        .expect("buffered Add reply must resolve")
        .expect("buffered Add reply sender must not be dropped");
    assert!(
        reply.is_ok(),
        "buffered Add must be registered before drain"
    );

    let outcome = tokio::time::timeout(Duration::from_secs(1), done_rx)
        .await
        .expect("watcher must resolve")
        .expect("watcher sender must not be dropped — the buffered Add must be acted on");
    assert!(
        matches!(outcome, TaskOutcome::Canceled | TaskOutcome::ForceAborted),
        "a buffered task drained at shutdown must terminate, got {outcome:?}"
    );

    assert!(
        reg.pending_joins.is_empty(),
        "wait_drained must leave no in-flight joins after shutdown"
    );

    let (reply, _reply_rx) = oneshot::channel();
    assert!(
        tx.try_send(RegistryCommand::Remove {
            id: TaskId::next(),
            reply,
        })
        .is_err(),
        "after shutdown the command channel is closed; sends must return Err"
    );
}
