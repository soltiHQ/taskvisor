//! Controller admission-policy integration tests (requires feature `controller`).

mod common;

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use taskvisor::prelude::*;
use taskvisor::{ControllerConfig, ControllerError, ControllerSpec, SlotStatusKind};

fn served_controller(cfg: ControllerConfig) -> (SupervisorHandle, Arc<EventCollector>) {
    let collector = EventCollector::new();
    let sup = Supervisor::builder(SupervisorConfig::default().with_grace(Duration::from_secs(5)))
        .with_subscribers(collector_subscribers(&collector))
        .with_controller(cfg)
        .build();
    (sup.serve(), collector)
}

async fn submit_running(handle: &SupervisorHandle, spec: ControllerSpec) -> TaskId {
    let task_name: Arc<str> = Arc::from(spec.task_spec().name());
    let id = handle
        .submit(spec)
        .await
        .expect("controller submission must be accepted");

    assert!(
        poll_until(Duration::from_secs(5), || async {
            handle.is_alive(&task_name).await
        })
        .await,
        "task {task_name:?} must reach the running registry state"
    );
    id
}

async fn expect_rejected(waiter: TaskWaiter) -> Arc<str> {
    match waiter.wait().await.expect("waiter errored") {
        TaskOutcome::Rejected { reason, .. } => reason,
        other => panic!("expected Rejected, got {other:?}"),
    }
}

fn logging_once(name: &str, log: Arc<Mutex<Vec<String>>>) -> TaskRef {
    let n = name.to_string();
    TaskFn::arc(name, move |_ctx: TaskContext| {
        let log = log.clone();
        let n = n.clone();
        async move {
            log.lock().unwrap().push(n);
            Ok(())
        }
    })
}

#[test]
fn controller_spec_components_are_configured_through_accessors() {
    let spec = ControllerSpec::queue(TaskSpec::once(make_ok_once("original")))
        .with_slot("shared")
        .with_admission(AdmissionPolicy::Replace)
        .with_task_spec(TaskSpec::once(make_ok_once("replacement")));

    assert_eq!(spec.admission(), AdmissionPolicy::Replace);
    assert_eq!(spec.task_spec().name(), "replacement");
    assert_eq!(spec.slot_override(), Some("shared"));
    assert_eq!(spec.slot_name(), "shared");

    let spec = spec.without_slot();
    assert_eq!(spec.slot_override(), None);
    assert_eq!(spec.slot_name(), "replacement");
    assert_eq!(spec.into_task_spec().name(), "replacement");
}

// Watched submission contracts.

#[tokio::test(flavor = "current_thread")]
async fn watched_submit_variants_resolve_completed_for_admitted_tasks() {
    let (handle, _collector) = served_controller(ControllerConfig::default());

    with_timeout(10, async {
        let (id, waiter) = handle
            .submit_and_watch(
                ControllerSpec::queue(TaskSpec::once(make_ok_once("watched-ok"))).with_slot("s"),
            )
            .await
            .expect("submit_and_watch ok");
        assert_eq!(waiter.id(), id);

        let outcome = waiter.wait().await.expect("waiter errored");
        assert!(
            matches!(outcome, TaskOutcome::Completed),
            "an admitted task that succeeds must resolve Completed, got {outcome:?}"
        );

        let (id, waiter) = handle
            .try_submit_and_watch(ControllerSpec::queue(TaskSpec::once(make_ok_once(
                "try-watched-ok",
            ))))
            .expect("the controller queue has capacity");
        assert_eq!(waiter.id(), id);
        assert!(matches!(waiter.wait().await, Ok(TaskOutcome::Completed)));

        handle.shutdown().await.expect("shutdown ok");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn submit_and_watch_resolves_rejected_on_drop_if_running() {
    let (handle, _collector) = served_controller(ControllerConfig::default());

    with_timeout(10, async {
        submit_running(
            &handle,
            ControllerSpec::queue(TaskSpec::restartable(make_coop("occupant-w"))).with_slot("s"),
        )
        .await;

        let (_id, waiter) = handle
            .submit_and_watch(
                ControllerSpec::drop_if_running(TaskSpec::restartable(make_coop("dropped-w")))
                    .with_slot("s"),
            )
            .await
            .expect("submit_and_watch accepted into channel");

        let reason = expect_rejected(waiter).await;
        assert!(
            reason.contains("dropped"),
            "rejection reason must explain why: {reason}"
        );

        handle.shutdown().await.expect("shutdown ok");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_immediately_removes_a_watched_queued_submission() {
    let (handle, _collector) = served_controller(ControllerConfig::default());

    with_timeout(10, async {
        submit_running(
            &handle,
            ControllerSpec::queue(TaskSpec::restartable(make_coop("occupant-rm"))).with_slot("s"),
        )
        .await;

        let (victim_id, waiter) = handle
            .submit_and_watch(
                ControllerSpec::queue(TaskSpec::restartable(make_coop("queued-victim-w")))
                    .with_slot("s"),
            )
            .await
            .expect("queued submit_and_watch ok");
        assert!(
            handle.cancel(victim_id).await.expect("cancel accepted"),
            "cancel must claim a queued controller submission even before observability catches up"
        );
        assert!(
            !handle
                .cancel(victim_id)
                .await
                .expect("second cancel must resolve"),
            "a queued submission can be claimed only once"
        );

        assert_eq!(&*expect_rejected(waiter).await, "removed_from_queue");

        handle.shutdown().await.expect("shutdown ok");
    })
    .await;
}

// Identity routing and event identity.

#[tokio::test(flavor = "current_thread")]
async fn direct_add_still_cancels_when_controller_is_configured() {
    let (handle, _collector) = served_controller(ControllerConfig::default());

    with_timeout(10, async {
        let id = handle
            .add(TaskSpec::restartable(make_coop("direct-cancel")))
            .await
            .expect("direct add must register");

        assert!(
            handle.cancel(id).await.expect("direct cancel must succeed"),
            "controller routing must fall through to the registry for a direct task"
        );
        assert!(handle.list().await.is_empty());

        handle.shutdown().await.expect("shutdown ok");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn remove_of_queued_submission_purges_it_before_start() {
    let (handle, collector) = served_controller(ControllerConfig::default());

    with_timeout(10, async {
        submit_running(
            &handle,
            ControllerSpec::queue(TaskSpec::restartable(make_coop("occupant-q"))).with_slot("s"),
        )
        .await;

        let victim_id = handle
            .submit(
                ControllerSpec::queue(TaskSpec::restartable(make_coop("queued-victim")))
                    .with_slot("s"),
            )
            .await
            .expect("second submit ok");

        assert!(
            collector
                .wait_until(Duration::from_secs(2), |events| {
                    events.iter().any(|event| {
                        event.kind == EventKind::ControllerSubmitted && event.id == Some(victim_id)
                    })
                })
                .await,
            "queued submission must be confirmed before removal"
        );

        assert!(
            handle.remove(victim_id).await.expect("remove accepted"),
            "remove must claim a queued controller submission"
        );
        assert!(
            collector
                .wait_until(Duration::from_secs(2), |events| {
                    events.iter().any(|event| {
                        event.kind == EventKind::ControllerRejected
                            && event.id == Some(victim_id)
                            && event.reason.as_deref() == Some("removed_from_queue")
                    })
                })
                .await,
            "controller must confirm the queued spec was purged"
        );

        assert!(
            handle
                .cancel_by_label("occupant-q")
                .await
                .expect("cancel occupant")
        );
        assert!(
            collector
                .wait_until(Duration::from_secs(2), |events| {
                    events.iter().any(|event| {
                        event.task.as_deref() == Some("occupant-q")
                            && event.kind == EventKind::TaskRemoved
                    })
                })
                .await,
            "the occupant must be removed before checking the purged queue"
        );
        assert!(
            collector
                .by_label("queued-victim")
                .iter()
                .all(|e| e.kind != EventKind::TaskStarting),
            "a removed queued submission must never start"
        );

        handle.shutdown().await.expect("shutdown ok");
    })
    .await;
}

// Shutdown and configuration edges.

#[tokio::test(flavor = "current_thread")]
async fn shutdown_does_not_start_queued_tasks() {
    let (handle, collector) = served_controller(ControllerConfig::default());

    with_timeout(10, async {
        submit_running(
            &handle,
            ControllerSpec::queue(TaskSpec::restartable(make_coop("occupant"))).with_slot("s"),
        )
        .await;

        handle
            .submit(
                ControllerSpec::queue(TaskSpec::restartable(make_coop("queued"))).with_slot("s"),
            )
            .await
            .expect("second submit ok");

        handle.shutdown().await.expect("shutdown ok");
    })
    .await;

    assert!(
        collector.by_label("queued").is_empty()
            || collector
                .by_label("queued")
                .iter()
                .all(|e| e.kind != EventKind::TaskStarting),
        "queued task must not start during shutdown"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn submit_without_controller_is_consistent_across_construction_paths() {
    let cases = [
        (
            "new",
            Supervisor::new(SupervisorConfig::default(), vec![]),
            ControllerSpec::queue(TaskSpec::once(make_ok_once("new"))),
        ),
        (
            "builder",
            Supervisor::builder(SupervisorConfig::default())
                .with_subscribers(vec![])
                .build(),
            ControllerSpec::drop_if_running(TaskSpec::once(make_ok_once("builder"))),
        ),
    ];

    with_timeout(5, async {
        for (constructor, supervisor, spec) in cases {
            let handle = supervisor.serve();

            assert_eq!(
                handle.submit(spec.clone()).await,
                Err(ControllerError::NotConfigured),
                "submit must reject a supervisor created through {constructor}"
            );
            assert_eq!(
                handle.try_submit(spec),
                Err(ControllerError::NotConfigured),
                "try_submit must reject a supervisor created through {constructor}"
            );
            assert!(handle.list().await.is_empty());
            handle.shutdown().await.expect("shutdown ok");
        }
    })
    .await;
}

// Admission policies and slot behavior.

#[tokio::test(flavor = "current_thread")]
async fn idle_submit_admits_emits_submitted_then_running_transition() {
    let (handle, collector) = served_controller(ControllerConfig::default());
    with_timeout(10, async {
        let spec = TaskSpec::restartable(make_coop("runner-7"));
        let id = submit_running(&handle, ControllerSpec::queue(spec).with_slot("web")).await;

        assert!(
            collector
                .wait_until(Duration::from_secs(3), |events| {
                    events.iter().any(|event| {
                        event.task.as_deref() == Some("web")
                            && event.kind == EventKind::ControllerSlotTransition
                            && event.reason.as_deref() == Some("admitting→running")
                    })
                })
                .await
        );

        let slot_events = collector.by_label("web");
        let submitted = slot_events
            .iter()
            .find(|event| {
                event.kind == EventKind::ControllerSubmitted
                    && event
                        .reason
                        .as_deref()
                        .is_some_and(|reason| reason.contains("status=admitting"))
            })
            .expect("the slot must publish its admitting submission");
        let running = slot_events
            .iter()
            .find(|event| {
                event.kind == EventKind::ControllerSlotTransition
                    && event.reason.as_deref() == Some("admitting→running")
            })
            .expect("the slot must publish its running transition");
        assert_eq!(submitted.id, Some(id));
        assert!(
            submitted.seq < running.seq,
            "ControllerSubmitted must precede the admitting→running transition"
        );
        assert!(
            collector
                .by_label("runner-7")
                .iter()
                .any(|event| event.kind == EventKind::TaskAdded && event.id == Some(id)),
            "TaskAdded must carry the id returned by submit()"
        );
        assert!(
            collector
                .by_label("runner-7")
                .iter()
                .any(|e| { e.kind == EventKind::TaskStarting && e.id == Some(id) }),
            "the lifecycle must run under the id minted at submit()"
        );

        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn queue_three_drains_in_fifo_order() {
    let (handle, _c) = served_controller(ControllerConfig::default());
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    with_timeout(10, async {
        for name in ["t1", "t2", "t3"] {
            let spec = TaskSpec::once(logging_once(name, log.clone()));
            handle
                .submit(ControllerSpec::queue(spec).with_slot("q"))
                .await
                .unwrap();
        }
        assert!(
            poll_until(Duration::from_secs(5), || async {
                log.lock().unwrap().len() == 3
            })
            .await
        );
        assert_eq!(*log.lock().unwrap(), vec!["t1", "t2", "t3"]);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn replace_supersedes_running_latest_wins() {
    let (handle, collector) = served_controller(ControllerConfig::default());
    with_timeout(10, async {
        let run1 = TaskSpec::restartable(make_coop("run-1"));
        submit_running(&handle, ControllerSpec::replace(run1).with_slot("s")).await;

        let run2 = TaskSpec::restartable(make_coop("run-2"));
        handle
            .submit(ControllerSpec::replace(run2).with_slot("s"))
            .await
            .unwrap();

        assert!(
            poll_until(Duration::from_secs(4), || async {
                let snap = handle.alive_snapshot().await;
                snap.iter().any(|n| &**n == "run-2") && !snap.iter().any(|n| &**n == "run-1")
            })
            .await,
            "latest-wins: run-2 alive, run-1 gone"
        );

        assert!(
            collector
                .wait_until(Duration::from_secs(2), |events| {
                    events.iter().any(|event| {
                        event.task.as_deref() == Some("s")
                            && event.kind == EventKind::ControllerSlotTransition
                            && event.reason.as_deref() == Some("running→terminating (replace)")
                    })
                })
                .await
        );
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn drop_if_running_rejects_busy_submission_without_starting_it() {
    let (handle, collector) = served_controller(ControllerConfig::default());
    with_timeout(10, async {
        let first = TaskSpec::restartable(make_coop("first"));
        submit_running(
            &handle,
            ControllerSpec::drop_if_running(first).with_slot("s"),
        )
        .await;

        let second = TaskSpec::restartable(make_coop("second"));
        let rejected_id = handle
            .submit(ControllerSpec::drop_if_running(second).with_slot("s"))
            .await
            .unwrap();

        assert!(
            collector
                .wait_until(Duration::from_secs(3), |events| {
                    events.iter().any(|event| {
                        event.task.as_deref() == Some("s")
                            && event.kind == EventKind::ControllerRejected
                            && event.id == Some(rejected_id)
                            && event.reason.as_deref().is_some_and(|reason| {
                                reason.starts_with(taskvisor::reasons::DROP_IF_RUNNING)
                            })
                    })
                })
                .await
        );
        assert!(
            !handle.is_alive("second").await,
            "busy slot must reject the second task"
        );
        assert!(handle.is_alive("first").await);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn drop_if_running_admits_when_slot_idle() {
    let (handle, collector) = served_controller(ControllerConfig::default());
    with_timeout(10, async {
        let solo = TaskSpec::restartable(make_coop("solo"));
        submit_running(
            &handle,
            ControllerSpec::drop_if_running(solo).with_slot("s"),
        )
        .await;

        assert!(
            collector
                .wait_until(Duration::from_secs(2), |events| {
                    events.iter().any(|event| {
                        event.task.as_deref() == Some("s")
                            && event.kind == EventKind::ControllerSubmitted
                    })
                })
                .await
        );
        assert_eq!(collector.count(EventKind::ControllerRejected), 0);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn distinct_slots_admit_tasks_independently() {
    let (handle, _c) = served_controller(ControllerConfig::default());
    with_timeout(10, async {
        let w1 = TaskSpec::restartable(make_coop("w1"));
        let w2 = TaskSpec::restartable(make_coop("w2"));
        handle
            .submit(ControllerSpec::queue(w1).with_slot("s1"))
            .await
            .unwrap();
        handle
            .submit(ControllerSpec::queue(w2).with_slot("s2"))
            .await
            .unwrap();

        assert!(
            poll_until(Duration::from_secs(4), || async {
                handle.is_alive("w1").await && handle.is_alive("w2").await
            })
            .await,
            "distinct slot keys run independently"
        );
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn submit_and_watch_duplicate_name_distinct_slots_resolves_rejected() {
    let (handle, _c) = served_controller(ControllerConfig::default());
    with_timeout(10, async {
        let first = TaskSpec::restartable(make_coop("dup"));
        submit_running(&handle, ControllerSpec::queue(first).with_slot("s1")).await;

        let (_id, waiter) = handle
            .submit_and_watch(
                ControllerSpec::queue(TaskSpec::restartable(make_coop("dup"))).with_slot("s2"),
            )
            .await
            .expect("second submit_and_watch accepted into channel");

        expect_rejected(waiter).await;

        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn replace_into_idle_slot_behaves_as_plain_admit() {
    let (handle, collector) = served_controller(ControllerConfig::default());
    with_timeout(10, async {
        let x = TaskSpec::restartable(make_coop("x"));
        submit_running(&handle, ControllerSpec::replace(x).with_slot("s")).await;

        assert!(
            collector
                .wait_until(Duration::from_secs(2), |events| {
                    events.iter().any(|event| {
                        event.task.as_deref() == Some("s")
                            && event.kind == EventKind::ControllerSlotTransition
                            && event.reason.as_deref() == Some("admitting→running")
                    })
                })
                .await
        );
        assert!(collector.by_label("s").iter().all(|e| {
            e.kind != EventKind::ControllerSlotTransition
                || e.reason.as_deref() != Some("running→terminating (replace)")
        }));
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn slot_freed_and_reusable_after_task_completes() {
    let (handle, _collector) = served_controller(ControllerConfig::default());
    with_timeout(10, async {
        let (_first_id, first) = handle
            .submit_and_watch(
                ControllerSpec::queue(TaskSpec::once(make_ok_once("first"))).with_slot("s"),
            )
            .await
            .expect("first submission accepted");
        assert!(matches!(first.wait().await, Ok(TaskOutcome::Completed)));

        submit_running(
            &handle,
            ControllerSpec::drop_if_running(TaskSpec::restartable(make_coop("second")))
                .with_slot("s"),
        )
        .await;
        let _ = handle.shutdown().await;
    })
    .await;
}

// Capacity, backpressure, and public snapshots.

#[tokio::test(flavor = "current_thread")]
async fn queue_full_rejects_with_controller_rejected_event() {
    let (handle, collector) = served_controller(ControllerConfig::default().with_max_slot_queue(1));
    with_timeout(10, async {
        let running = TaskSpec::restartable(make_coop("r"));
        submit_running(&handle, ControllerSpec::queue(running).with_slot("s")).await;

        let p1 = TaskSpec::restartable(make_coop("p1"));
        let p2 = TaskSpec::restartable(make_coop("p2"));
        handle
            .submit(ControllerSpec::queue(p1).with_slot("s"))
            .await
            .unwrap();
        handle
            .submit(ControllerSpec::queue(p2).with_slot("s"))
            .await
            .unwrap();
        assert!(
            collector
                .wait_until(Duration::from_secs(3), |events| {
                    events.iter().any(|event| {
                        event.task.as_deref() == Some("s")
                            && event.kind == EventKind::ControllerRejected
                            && event
                                .reason
                                .as_deref()
                                .is_some_and(|reason| reason.contains("queue_full"))
                    })
                })
                .await
        );
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn try_submit_full_when_queue_capacity_saturated() {
    let (handle, _c) = served_controller(
        ControllerConfig::default().with_queue_capacity(NonZeroUsize::new(1).unwrap()),
    );
    with_timeout(10, async {
        let mut saw_full = false;
        for _ in 0..256 {
            let spec = TaskSpec::once(make_ok_once("q"));
            if let Err(ControllerError::Full) =
                handle.try_submit(ControllerSpec::queue(spec).with_slot("q"))
            {
                saw_full = true;
                break;
            }
        }
        assert!(
            saw_full,
            "saturated intake channel must yield ControllerError::Full"
        );
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn controller_snapshot_reports_running_slot_and_queue_depth() {
    let (handle, _collector) =
        served_controller(ControllerConfig::new(NonZeroUsize::new(16).unwrap(), 4));

    with_timeout(10, async {
        let occupant_id = handle
            .submit(
                ControllerSpec::queue(TaskSpec::restartable(make_coop("occupant-snap")))
                    .with_slot("s"),
            )
            .await
            .expect("submit occupant ok");
        handle
            .submit(
                ControllerSpec::queue(TaskSpec::restartable(make_coop("queued-snap")))
                    .with_slot("s"),
            )
            .await
            .expect("submit queued ok");

        let observed = poll_until(Duration::from_secs(5), || async {
            let Some(snapshot) = handle.controller_snapshot().await else {
                return false;
            };
            let Some(view) = snapshot.slot("s") else {
                return false;
            };

            view.status == SlotStatusKind::Running
                && view.owner_id == Some(occupant_id)
                && view.queue_depth == 1
                && snapshot.running_count() == 1
                && snapshot.total_queued() == 1
        })
        .await;
        assert!(
            observed,
            "controller_snapshot must report slot 's' Running with queue_depth 1"
        );

        handle.shutdown().await.expect("shutdown ok");
    })
    .await;
}

// Controller lifecycle integration.

#[tokio::test(flavor = "current_thread")]
async fn natural_run_joins_controller_before_return() {
    let sup = Supervisor::builder(SupervisorConfig::default())
        .with_controller(ControllerConfig::default())
        .build();

    with_timeout(5, async {
        sup.run(vec![])
            .await
            .expect("empty run must finish cleanly");

        let handle = sup.serve();
        let result = handle.try_submit(ControllerSpec::queue(TaskSpec::once(make_ok_once(
            "after-natural-shutdown",
        ))));
        assert_eq!(result, Err(ControllerError::Closed));
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_static_batch_keeps_controller_running() {
    let sup = Supervisor::builder(SupervisorConfig::default())
        .with_controller(ControllerConfig::default())
        .build();
    let first = TaskSpec::once(make_ok_once("duplicate-static"));
    let second = TaskSpec::once(make_ok_once("duplicate-static"));

    with_timeout(5, async {
        assert!(matches!(
            sup.run(vec![first, second]).await,
            Err(RuntimeError::TaskAlreadyExists { .. })
        ));

        let handle = sup.serve();
        let (_id, waiter) = handle
            .submit_and_watch(ControllerSpec::queue(TaskSpec::once(make_ok_once(
                "after-rejected-static-batch",
            ))))
            .await
            .expect("batch rejection must not stop controller intake");
        assert!(matches!(waiter.wait().await, Ok(TaskOutcome::Completed)));
        handle
            .shutdown()
            .await
            .expect("shutdown must join controller");
    })
    .await;
}
