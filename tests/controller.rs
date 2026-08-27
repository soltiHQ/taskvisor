//! Controller admission-policy integration tests (requires feature `controller`).

mod common;

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use taskvisor::prelude::*;
use taskvisor::{ControllerConfig, ControllerError, ControllerSpec, SlotStatusKind};
use tokio::sync::Notify;

fn served_controller(cfg: ControllerConfig) -> (SupervisorHandle, Arc<EventCollector>) {
    let collector = EventCollector::new();
    let sup = Supervisor::builder(SupervisorConfig::default().with_grace(Duration::from_secs(5)))
        .with_subscribers(collector_subscribers(&collector))
        .with_controller(cfg)
        .build();
    (sup.serve().expect("runtime startup"), collector)
}

async fn submit_running(handle: &SupervisorHandle, spec: ControllerSpec) -> TaskId {
    let task_name: Arc<str> = Arc::from(spec.task_spec().name());
    let id = handle
        .submit(spec)
        .execute()
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

async fn expect_rejected(waiter: TaskWaiter) -> RejectionKind {
    match waiter.wait().await.expect("waiter errored") {
        TaskOutcome::Rejected { kind, .. } => kind,
        other => panic!("expected Rejected, got {other:?}"),
    }
}

fn logging_once(entry: &str, log: Arc<Mutex<Vec<String>>>) -> TaskRef {
    let entry = entry.to_string();
    TaskFn::arc(move |_ctx: TaskContext| {
        let log = log.clone();
        let entry = entry.clone();
        async move {
            log.lock().unwrap().push(entry);
            Ok(())
        }
    })
}

#[test]
fn controller_spec_components_are_configured_through_accessors() {
    let spec = ControllerSpec::queue(TaskSpec::once("original", make_ok_once()))
        .with_slot("shared")
        .with_admission(AdmissionPolicy::Replace)
        .with_task_spec(TaskSpec::once("replacement", make_ok_once()));

    assert_eq!(spec.admission(), AdmissionPolicy::Replace);
    assert_eq!(spec.task_spec().name(), "replacement");
    assert_eq!(spec.slot_override(), Some("shared"));
    assert_eq!(spec.slot_name(), "shared");

    let spec = spec.without_slot();
    assert_eq!(spec.slot_override(), None);
    assert_eq!(spec.slot_name(), "replacement");
    assert_eq!(spec.into_task_spec().name(), "replacement");
}

#[test]
fn runtime_drop_before_first_controller_poll_closes_command_channel() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");

    let (supervisor, handle, preaccepted) = runtime.block_on(async {
        let supervisor = Supervisor::builder(SupervisorConfig::default())
            .with_controller(ControllerConfig::default())
            .build();
        let handle = supervisor.serve().expect("runtime startup");
        let waiter = handle
            .submit(ControllerSpec::queue(TaskSpec::once(
                "before-first-controller-poll",
                make_ok_once(),
            )))
            .watch()
            .try_intake()
            .expect("the open command channel accepts work before the controller is first polled");
        (supervisor, handle, waiter)
    });

    drop(runtime);

    let late = handle
        .submit(ControllerSpec::queue(TaskSpec::once(
            "after-controller-runtime-drop",
            make_ok_once(),
        )))
        .watch()
        .try_intake();
    assert!(matches!(late, Err(ControllerError::Closed)));

    let outcome_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("outcome runtime");
    let preaccepted = outcome_runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(1), preaccepted.wait())
            .await
            .expect("receiver teardown must resolve the preaccepted waiter")
    });
    assert!(matches!(
        preaccepted,
        Err(RuntimeError::OutcomeUnavailable { .. })
    ));

    drop(outcome_runtime);
    drop(handle);
    drop(supervisor);
}

#[tokio::test(flavor = "current_thread")]
async fn prepared_submission_exposes_identity_before_events_and_preserves_it() {
    let (handle, collector) = served_controller(ControllerConfig::default());

    with_timeout(10, async {
        let request = ControllerSpec::queue(TaskSpec::once("prepared-watched", make_ok_once()))
            .with_slot("prepared-slot");
        let prepared = handle
            .prepare_submission(request)
            .expect("controller is configured");
        let reserved_id = prepared.id();

        assert_eq!(prepared.spec().slot_name(), "prepared-slot");
        assert!(
            collector.by_id(reserved_id).is_empty(),
            "preparation must not publish an event"
        );

        let waiter = prepared
            .submit()
            .watch()
            .execute()
            .await
            .expect("prepared submission must enter the controller queue");
        let submitted_id = waiter.id();
        assert_eq!(submitted_id, reserved_id);
        assert_eq!(waiter.id(), reserved_id);
        assert!(matches!(waiter.wait().await, Ok(TaskOutcome::Completed)));

        assert!(
            collector
                .wait_until(Duration::from_secs(2), |events| {
                    events.iter().any(|event| {
                        event.id == Some(reserved_id) && event.kind == EventKind::TaskRemoved
                    })
                })
                .await,
            "the prepared identity must be used through terminal cleanup"
        );
        assert!(
            collector.by_id(reserved_id).iter().any(|event| {
                event.kind == EventKind::AttemptStarting && event.attempt == Some(1)
            })
        );

        handle.shutdown().await.expect("shutdown ok");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn controller_ownership_timeouts_cover_all_public_submission_paths() {
    let collector = EventCollector::new();
    let config = SupervisorConfig::default()
        .try_with_ownership_capacity(2)
        .expect("the subscriber and one task each need one ownership unit");
    let supervisor = Supervisor::builder(config)
        .with_subscribers(collector_subscribers(&collector))
        .with_controller(ControllerConfig::default())
        .build();
    let handle = supervisor.serve().expect("runtime startup");
    let holder_waiter = handle
        .add(TaskSpec::once(
            "controller-ownership-timeout-holder",
            make_coop(),
        ))
        .watch()
        .execute()
        .await
        .expect("the holder must consume the remaining ownership unit");
    let holder_id = holder_waiter.id();
    assert!(
        poll_until(Duration::from_secs(2), || async {
            handle.is_alive("controller-ownership-timeout-holder").await
        })
        .await,
        "the holder must start before saturation is tested"
    );

    let error = handle
        .submit(ControllerSpec::queue(TaskSpec::once(
            "controller-ownership-timeout-submit",
            make_ok_once(),
        )))
        .ownership_timeout(Duration::ZERO)
        .execute()
        .await
        .expect_err("a saturated submission must time out");
    assert!(matches!(
        error,
        ControllerError::OwnershipAdmissionTimeout { timeout, .. }
            if timeout == Duration::ZERO
    ));

    let error = handle
        .submit(ControllerSpec::queue(TaskSpec::once(
            "controller-ownership-timeout-submit-watched",
            make_ok_once(),
        )))
        .watch()
        .ownership_timeout(Duration::ZERO)
        .execute()
        .await
        .expect_err("a saturated watched submission must time out");
    assert!(matches!(
        error,
        ControllerError::OwnershipAdmissionTimeout { timeout, .. }
            if timeout == Duration::ZERO
    ));

    let prepared = handle
        .prepare_submission(ControllerSpec::queue(TaskSpec::once(
            "controller-ownership-timeout-prepared",
            make_ok_once(),
        )))
        .expect("controller configured");
    let prepared_id = prepared.id();
    let error = prepared
        .submit()
        .ownership_timeout(Duration::ZERO)
        .execute()
        .await
        .expect_err("a saturated prepared submission must time out");
    assert!(matches!(
        error,
        ControllerError::OwnershipAdmissionTimeout { timeout, .. }
            if timeout == Duration::ZERO
    ));

    let prepared_watched = handle
        .prepare_submission(ControllerSpec::queue(TaskSpec::once(
            "controller-ownership-timeout-prepared-watched",
            make_ok_once(),
        )))
        .expect("controller configured");
    let prepared_watched_id = prepared_watched.id();
    let error = prepared_watched
        .submit()
        .watch()
        .ownership_timeout(Duration::ZERO)
        .execute()
        .await
        .expect_err("a saturated watched prepared submission must time out");
    assert!(matches!(
        error,
        ControllerError::OwnershipAdmissionTimeout { timeout, .. }
            if timeout == Duration::ZERO
    ));

    let saturated = handle.ownership_snapshot();
    assert_eq!(saturated.available, Some(0));
    assert_eq!(saturated.waiters, 0);
    assert!(
        handle
            .controller_snapshot()
            .await
            .expect("controller configured")
            .slots
            .is_empty(),
        "timed-out submissions must not enter controller state"
    );

    assert!(
        handle
            .cancel(holder_id)
            .execute()
            .await
            .expect("cancel holder")
    );
    assert!(matches!(
        with_timeout(2, holder_waiter.wait()).await,
        Ok(TaskOutcome::Canceled)
    ));
    assert!(
        poll_until(Duration::from_secs(2), || async {
            handle.ownership_snapshot().available == Some(1)
        })
        .await,
        "the holder cleanup must return its ownership unit"
    );

    let marker_waiter = handle
        .submit(ControllerSpec::queue(TaskSpec::once(
            "controller-ownership-timeout-marker",
            make_ok_once(),
        )))
        .watch()
        .ownership_timeout(Duration::ZERO)
        .execute()
        .await
        .expect("an immediately available ownership unit must beat a zero deadline");
    assert!(matches!(
        with_timeout(2, marker_waiter.wait()).await,
        Ok(TaskOutcome::Completed)
    ));
    assert!(
        collector
            .wait_until(Duration::from_secs(2), |events| {
                events.iter().any(|event| {
                    event.kind == EventKind::TaskRemoved
                        && event.task.as_deref() == Some("controller-ownership-timeout-marker")
                })
            })
            .await,
        "the marker event must flush earlier lifecycle events"
    );
    for name in [
        "controller-ownership-timeout-submit",
        "controller-ownership-timeout-submit-watched",
        "controller-ownership-timeout-prepared",
        "controller-ownership-timeout-prepared-watched",
    ] {
        assert!(
            collector.by_name(name).is_empty(),
            "timed-out submission {name} must not emit lifecycle events"
        );
    }
    assert!(collector.by_id(prepared_id).is_empty());
    assert!(collector.by_id(prepared_watched_id).is_empty());

    handle.shutdown().await.expect("runtime shutdown");
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_prepared_submission_starts_no_work_and_publishes_no_event() {
    let (handle, collector) = served_controller(ControllerConfig::default());
    let starts = Arc::new(AtomicUsize::new(0));
    let task_starts = Arc::clone(&starts);
    let task = TaskFn::arc(move |_ctx| {
        let starts = Arc::clone(&task_starts);
        async move {
            starts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    });

    with_timeout(10, async {
        let prepared = handle
            .prepare_submission(
                ControllerSpec::queue(TaskSpec::once("prepared-dropped", task))
                    .with_slot("prepared-dropped"),
            )
            .expect("controller is configured");
        let dropped_id = prepared.id();
        drop(prepared);

        let barrier = handle
            .submit(ControllerSpec::queue(TaskSpec::once(
                "prepared-drop-barrier",
                make_ok_once(),
            )))
            .watch()
            .execute()
            .await
            .expect("barrier submission");
        let barrier_id = barrier.id();
        assert!(matches!(barrier.wait().await, Ok(TaskOutcome::Completed)));
        assert!(
            collector
                .wait_until(Duration::from_secs(2), |events| {
                    events.iter().any(|event| {
                        event.id == Some(barrier_id) && event.kind == EventKind::TaskRemoved
                    })
                })
                .await
        );

        assert_eq!(starts.load(Ordering::SeqCst), 0);
        assert!(collector.by_id(dropped_id).is_empty());

        handle.shutdown().await.expect("shutdown ok");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn watched_submit_variants_resolve_completed_for_admitted_tasks() {
    let (handle, _collector) = served_controller(ControllerConfig::default());

    with_timeout(10, async {
        let waiter = handle
            .submit(
                ControllerSpec::queue(TaskSpec::once("watched-ok", make_ok_once())).with_slot("s"),
            )
            .watch()
            .execute()
            .await
            .expect("watched submit ok");
        let id = waiter.id();
        assert_eq!(waiter.id(), id);

        let outcome = waiter.wait().await.expect("waiter errored");
        assert!(
            matches!(outcome, TaskOutcome::Completed),
            "an admitted task that succeeds must resolve Completed, got {outcome:?}"
        );

        let prepared = handle
            .prepare_submission(ControllerSpec::queue(TaskSpec::once(
                "try-watched-ok",
                make_ok_once(),
            )))
            .expect("controller is configured");
        let reserved_id = prepared.id();
        let waiter = prepared
            .submit()
            .watch()
            .try_intake()
            .expect("the controller queue has capacity");
        let id = waiter.id();
        assert_eq!(id, reserved_id);
        assert_eq!(waiter.id(), id);
        assert!(matches!(waiter.wait().await, Ok(TaskOutcome::Completed)));

        handle.shutdown().await.expect("shutdown ok");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn watched_submit_resolves_rejected_on_drop_if_running() {
    let (handle, collector) = served_controller(ControllerConfig::default());

    with_timeout(10, async {
        submit_running(
            &handle,
            ControllerSpec::queue(TaskSpec::restartable("occupant-w", make_coop())).with_slot("s"),
        )
        .await;

        let waiter = handle
            .submit(
                ControllerSpec::drop_if_running(TaskSpec::restartable("dropped-w", make_coop()))
                    .with_slot("s"),
            )
            .watch()
            .execute()
            .await
            .expect("watched submit accepted into channel");
        let id = waiter.id();

        let outcome = waiter.wait().await.expect("waiter errored");
        assert!(matches!(
            outcome,
            TaskOutcome::Rejected {
                kind: RejectionKind::SlotBusy,
                ..
            }
        ));
        assert!(
            collector
                .wait_until(Duration::from_secs(2), |events| {
                    events.iter().any(|event| {
                        event.id == Some(id) && event.kind == EventKind::ControllerRejected
                    })
                })
                .await
        );
        let by_id = collector.by_id(id);
        assert!(by_id.iter().any(|event| {
            event.kind == EventKind::ControllerRejected
                && event.outcome_kind == Some(TaskOutcomeKind::Rejected)
                && event.rejection_kind == Some(RejectionKind::SlotBusy)
        }));
        assert!(by_id.iter().all(|event| {
            !matches!(
                event.kind,
                EventKind::TaskAdded
                    | EventKind::AttemptStarting
                    | EventKind::TaskFinished
                    | EventKind::TaskRemoved
            )
        }));

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
            ControllerSpec::queue(TaskSpec::restartable("occupant-rm", make_coop())).with_slot("s"),
        )
        .await;

        let waiter = handle
            .submit(
                ControllerSpec::queue(TaskSpec::restartable("queued-victim-w", make_coop()))
                    .with_slot("s"),
            )
            .watch()
            .execute()
            .await
            .expect("queued watched submit ok");
        let victim_id = waiter.id();
        assert!(
            handle
                .cancel(victim_id)
                .execute()
                .await
                .expect("cancel accepted"),
            "cancel must claim a queued controller submission even before observability catches up"
        );
        assert!(
            !handle
                .cancel(victim_id)
                .execute()
                .await
                .expect("second cancel must resolve"),
            "a queued submission can be claimed only once"
        );

        assert_eq!(
            expect_rejected(waiter).await,
            RejectionKind::RemovedFromQueue
        );

        handle.shutdown().await.expect("shutdown ok");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn direct_add_still_cancels_when_controller_is_configured() {
    let (handle, _collector) = served_controller(ControllerConfig::default());

    with_timeout(10, async {
        let id = handle
            .add(TaskSpec::restartable("direct-cancel", make_coop()))
            .execute()
            .await
            .expect("direct add must register");

        assert!(
            handle
                .cancel(id)
                .execute()
                .await
                .expect("direct cancel must succeed"),
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
            ControllerSpec::queue(TaskSpec::restartable("occupant-q", make_coop())).with_slot("s"),
        )
        .await;

        let victim_id = handle
            .submit(
                ControllerSpec::queue(TaskSpec::restartable("queued-victim", make_coop()))
                    .with_slot("s"),
            )
            .execute()
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
            handle
                .remove(victim_id)
                .execute()
                .await
                .expect("remove accepted"),
            "remove must claim a queued controller submission"
        );
        assert!(
            collector
                .wait_until(Duration::from_secs(2), |events| {
                    events.iter().any(|event| {
                        event.kind == EventKind::ControllerRejected
                            && event.id == Some(victim_id)
                            && event.rejection_kind == Some(RejectionKind::RemovedFromQueue)
                            && event.outcome_kind == Some(TaskOutcomeKind::Rejected)
                    })
                })
                .await,
            "controller must confirm the queued spec was purged"
        );

        assert!(
            handle
                .cancel("occupant-q")
                .execute()
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
                .by_name("queued-victim")
                .iter()
                .all(|e| e.kind != EventKind::AttemptStarting),
            "a removed queued submission must never start"
        );

        handle.shutdown().await.expect("shutdown ok");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_does_not_start_queued_tasks() {
    let (handle, collector) = served_controller(ControllerConfig::default());

    with_timeout(10, async {
        submit_running(
            &handle,
            ControllerSpec::queue(TaskSpec::restartable("occupant", make_coop())).with_slot("s"),
        )
        .await;

        handle
            .submit(
                ControllerSpec::queue(TaskSpec::restartable("queued", make_coop())).with_slot("s"),
            )
            .execute()
            .await
            .expect("second submit ok");

        handle.shutdown().await.expect("shutdown ok");
    })
    .await;

    assert!(
        collector.by_name("queued").is_empty()
            || collector
                .by_name("queued")
                .iter()
                .all(|e| e.kind != EventKind::AttemptStarting),
        "queued task must not start during shutdown"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn submit_without_controller_is_consistent_across_construction_paths() {
    let cases = [
        (
            "new",
            Supervisor::new(SupervisorConfig::default(), vec![]),
            ControllerSpec::queue(TaskSpec::once("new", make_ok_once())),
        ),
        (
            "builder",
            Supervisor::builder(SupervisorConfig::default())
                .with_subscribers(vec![])
                .build(),
            ControllerSpec::drop_if_running(TaskSpec::once("builder", make_ok_once())),
        ),
    ];

    with_timeout(5, async {
        for (constructor, supervisor, spec) in cases {
            let handle = supervisor.serve().expect("runtime startup");

            assert!(
                matches!(
                    handle.prepare_submission(spec.clone()),
                    Err(ControllerError::NotConfigured)
                ),
                "prepare_submission must reject a supervisor created through {constructor}"
            );

            assert_eq!(
                handle.submit(spec.clone()).execute().await,
                Err(ControllerError::NotConfigured),
                "submit must reject a supervisor created through {constructor}"
            );
            assert_eq!(
                handle
                    .submit(spec.clone())
                    .ownership_timeout(Duration::ZERO)
                    .execute()
                    .await,
                Err(ControllerError::NotConfigured),
                "timed submit must reject a supervisor created through {constructor}"
            );
            assert!(
                matches!(
                    handle
                        .submit(spec.clone())
                        .watch()
                        .ownership_timeout(Duration::ZERO)
                        .execute()
                        .await,
                    Err(ControllerError::NotConfigured)
                ),
                "timed watched submit must reject a supervisor created through {constructor}"
            );
            assert_eq!(
                handle.submit(spec).try_intake(),
                Err(ControllerError::NotConfigured),
                "submit try_intake must reject a supervisor created through {constructor}"
            );
            assert!(handle.list().await.is_empty());
            handle.shutdown().await.expect("shutdown ok");
        }
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn idle_submit_admits_emits_submitted_then_running_transition() {
    let (handle, collector) = served_controller(ControllerConfig::default());
    with_timeout(10, async {
        let spec = TaskSpec::restartable("runner-7", make_coop());
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

        let slot_events = collector.by_name("web");
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
                .by_name("runner-7")
                .iter()
                .any(|event| event.kind == EventKind::TaskAdded && event.id == Some(id)),
            "TaskAdded must carry the id returned by submit()"
        );
        assert!(
            collector
                .by_name("runner-7")
                .iter()
                .any(|e| { e.kind == EventKind::AttemptStarting && e.id == Some(id) }),
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
            let spec = TaskSpec::once(name, logging_once(name, log.clone()));
            handle
                .submit(ControllerSpec::queue(spec).with_slot("q"))
                .execute()
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slot_waits_for_force_reaped_owner_before_starting_different_name() {
    let supervisor =
        Supervisor::builder(SupervisorConfig::default().with_grace(Duration::from_millis(20)))
            .with_controller(ControllerConfig::default())
            .build();
    let handle = supervisor.serve().expect("runtime startup");
    let release = Arc::new(AtomicBool::new(false));
    let _release_on_drop = ReleaseBlockedPoll(Arc::clone(&release));
    let started = Arc::new(Notify::new());

    let owner_waiter = handle
        .submit(
            ControllerSpec::queue(TaskSpec::restartable(
                "physical-owner-a",
                synchronously_blocked_task(Arc::clone(&release), Arc::clone(&started)),
            ))
            .with_slot("physical-slot-a"),
        )
        .watch()
        .execute()
        .await
        .expect("the blocking owner must enter controller intake");
    let owner_id = owner_waiter.id();
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("the owner must enter its synchronous poll");

    let next_runs = Arc::new(AtomicUsize::new(0));
    let runs = Arc::clone(&next_runs);
    let next = TaskFn::arc(move |_ctx| {
        let runs = Arc::clone(&runs);
        async move {
            runs.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    });
    let next_waiter = handle
        .submit(
            ControllerSpec::queue(TaskSpec::once("physical-next-b", next))
                .with_slot("physical-slot-a"),
        )
        .watch()
        .execute()
        .await
        .expect("the next task must enter the same slot queue");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            handle
                .controller_snapshot()
                .await
                .and_then(|snapshot| snapshot.slot("physical-slot-a").cloned())
                .is_some_and(|slot| slot.queue_depth == 1)
        })
        .await,
        "the next task must be queued before owner cancellation"
    );

    assert!(
        handle
            .cancel(owner_id)
            .execute()
            .await
            .expect("cancel blocked owner")
    );
    assert!(matches!(
        owner_waiter.wait().await,
        Ok(TaskOutcome::ForceAborted)
    ));
    let mut next_outcome = Box::pin(next_waiter.wait());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), next_outcome.as_mut())
            .await
            .is_err(),
        "logical force-abort must not release the controller slot"
    );
    assert_eq!(next_runs.load(Ordering::SeqCst), 0);

    release.store(true, Ordering::Release);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), next_outcome).await,
        Ok(Ok(TaskOutcome::Completed))
    ));
    assert_eq!(next_runs.load(Ordering::SeqCst), 1);
    handle.shutdown().await.expect("shutdown ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slot_waits_for_force_reaped_owner_before_readmitting_same_name() {
    let supervisor =
        Supervisor::builder(SupervisorConfig::default().with_grace(Duration::from_millis(20)))
            .with_controller(ControllerConfig::default())
            .build();
    let handle = supervisor.serve().expect("runtime startup");
    let release = Arc::new(AtomicBool::new(false));
    let _release_on_drop = ReleaseBlockedPoll(Arc::clone(&release));
    let started = Arc::new(Notify::new());
    let name = "physical-same-label";

    let owner_waiter = handle
        .submit(
            ControllerSpec::queue(TaskSpec::restartable(
                name,
                synchronously_blocked_task(Arc::clone(&release), Arc::clone(&started)),
            ))
            .with_slot("physical-slot-same"),
        )
        .watch()
        .execute()
        .await
        .expect("the blocking owner must enter controller intake");
    let owner_id = owner_waiter.id();
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("the owner must enter its synchronous poll");

    let replacement_runs = Arc::new(AtomicUsize::new(0));
    let runs = Arc::clone(&replacement_runs);
    let replacement = TaskFn::arc(move |_ctx| {
        let runs = Arc::clone(&runs);
        async move {
            runs.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    });
    let replacement_waiter = handle
        .submit(
            ControllerSpec::queue(TaskSpec::once(name, replacement))
                .with_slot("physical-slot-same"),
        )
        .watch()
        .execute()
        .await
        .expect("the replacement must enter the same slot queue");
    assert!(
        handle
            .cancel(owner_id)
            .execute()
            .await
            .expect("cancel blocked owner")
    );
    assert!(matches!(
        owner_waiter.wait().await,
        Ok(TaskOutcome::ForceAborted)
    ));

    let mut replacement_outcome = Box::pin(replacement_waiter.wait());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), replacement_outcome.as_mut())
            .await
            .is_err(),
        "the queued same-name task must wait while the reaper reserves its name"
    );
    assert_eq!(replacement_runs.load(Ordering::SeqCst), 0);

    release.store(true, Ordering::Release);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), replacement_outcome).await,
        Ok(Ok(TaskOutcome::Completed))
    ));
    assert_eq!(replacement_runs.load(Ordering::SeqCst), 1);
    handle.shutdown().await.expect("shutdown ok");
}

#[tokio::test(flavor = "current_thread")]
async fn replace_supersedes_running_latest_wins() {
    let (handle, collector) = served_controller(ControllerConfig::default());
    with_timeout(10, async {
        let run1 = TaskSpec::restartable("run-1", make_coop());
        submit_running(&handle, ControllerSpec::replace(run1).with_slot("s")).await;

        let run2 = TaskSpec::restartable("run-2", make_coop());
        handle
            .submit(ControllerSpec::replace(run2).with_slot("s"))
            .execute()
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
        let first = TaskSpec::restartable("first", make_coop());
        submit_running(
            &handle,
            ControllerSpec::drop_if_running(first).with_slot("s"),
        )
        .await;

        let second = TaskSpec::restartable("second", make_coop());
        let rejected_id = handle
            .submit(ControllerSpec::drop_if_running(second).with_slot("s"))
            .execute()
            .await
            .unwrap();

        assert!(
            collector
                .wait_until(Duration::from_secs(3), |events| {
                    events.iter().any(|event| {
                        event.task.as_deref() == Some("s")
                            && event.kind == EventKind::ControllerRejected
                            && event.id == Some(rejected_id)
                            && event.outcome_kind == Some(TaskOutcomeKind::Rejected)
                            && event.rejection_kind == Some(RejectionKind::SlotBusy)
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
        let solo = TaskSpec::restartable("solo", make_coop());
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
        let w1 = TaskSpec::restartable("w1", make_coop());
        let w2 = TaskSpec::restartable("w2", make_coop());
        handle
            .submit(ControllerSpec::queue(w1).with_slot("s1"))
            .execute()
            .await
            .unwrap();
        handle
            .submit(ControllerSpec::queue(w2).with_slot("s2"))
            .execute()
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
async fn watched_submit_duplicate_name_distinct_slots_resolves_rejected() {
    let (handle, _c) = served_controller(ControllerConfig::default());
    with_timeout(10, async {
        let first = TaskSpec::restartable("dup", make_coop());
        submit_running(&handle, ControllerSpec::queue(first).with_slot("s1")).await;

        let waiter = handle
            .submit(
                ControllerSpec::queue(TaskSpec::restartable("dup", make_coop())).with_slot("s2"),
            )
            .watch()
            .execute()
            .await
            .expect("second watched submit accepted into channel");

        expect_rejected(waiter).await;

        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn replace_into_idle_slot_behaves_as_plain_admit() {
    let (handle, collector) = served_controller(ControllerConfig::default());
    with_timeout(10, async {
        let x = TaskSpec::restartable("x", make_coop());
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
        assert!(collector.by_name("s").iter().all(|e| {
            e.kind != EventKind::ControllerSlotTransition
                || e.reason.as_deref() != Some("running→terminating (replace)")
        }));
        let _ = handle.shutdown().await;
    })
    .await;
}

async fn wait_for_controller_slot_release(handle: &SupervisorHandle, slot: &str) {
    let mut last_view = None;
    let released = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            last_view = handle
                .controller_snapshot()
                .await
                .expect("controller is configured")
                .slot(slot)
                .cloned();
            if last_view.as_ref().is_none_or(|view| {
                view.status == SlotStatusKind::Idle
                    && view.owner_id.is_none()
                    && view.queue_depth == 0
            }) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        released.is_ok(),
        "slot {slot:?} did not release without another submission: {last_view:?}"
    );
}

async fn assert_slot_freed_and_reusable_after_task_completes() {
    let (handle, _collector) = served_controller(ControllerConfig::default());
    with_timeout(10, async {
        let first = handle
            .submit(
                ControllerSpec::drop_if_running(TaskSpec::once("first", make_ok_once()))
                    .with_slot("s"),
            )
            .watch()
            .execute()
            .await
            .expect("first submission accepted");
        assert!(matches!(first.wait().await, Ok(TaskOutcome::Completed)));
        wait_for_controller_slot_release(&handle, "s").await;

        let second = handle
            .submit(
                ControllerSpec::drop_if_running(TaskSpec::once("second", make_ok_once()))
                    .with_slot("s"),
            )
            .watch()
            .execute()
            .await
            .expect("single submission into the released slot");
        assert!(matches!(second.wait().await, Ok(TaskOutcome::Completed)));
        wait_for_controller_slot_release(&handle, "s").await;
        handle.shutdown().await.expect("reusable slot shutdown");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn slot_freed_and_reusable_after_task_completes() {
    assert_slot_freed_and_reusable_after_task_completes().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slot_freed_and_reusable_after_task_completes_multi_thread() {
    assert_slot_freed_and_reusable_after_task_completes().await;
}

/// The initial immediate-reuse diagnostic failed on pair 4: ordinary Completed,
/// then SlotBusy with no candidate start, then autonomous slot release.
async fn audit_drop_admission_after_ordinary_completion(runtime: &str) {
    const PAIRS: usize = 2_048;
    let (handle, _collector) = served_controller(ControllerConfig::default());
    with_timeout(30, async {
        let mut immediate_completed = 0;
        let mut prior_slot_busy = 0;
        let mut completed_after_release = 0;
        let mut first_rejection = None;
        for iteration in 0..PAIRS {
            let slot = format!("completed-drop-{iteration}");
            let owner_name = format!("completed-owner-{iteration}");
            let candidate_name = format!("completed-candidate-{iteration}");
            let candidate_starts = Arc::new(AtomicUsize::new(0));
            let starts = Arc::clone(&candidate_starts);
            let candidate = TaskFn::arc(move |_ctx| {
                starts.fetch_add(1, Ordering::AcqRel);
                async { Ok(()) }
            });
            let candidate = handle
                .prepare_submission(
                    ControllerSpec::drop_if_running(TaskSpec::once(candidate_name, candidate))
                        .with_slot(slot.clone()),
                )
                .expect("prepare candidate");
            let owner = handle
                .submit(ControllerSpec::drop_if_running(TaskSpec::once(
                        owner_name.clone(),
                        make_ok_once(),
                    ))
                    .with_slot(slot.clone())).watch().execute()
                .await
                .expect("submit owner into a fresh slot");
            let owner_id = owner.id();
            assert!(matches!(owner.wait().await, Ok(TaskOutcome::Completed)));

            let outcome = candidate
                .submit().watch().execute()
                .await
                .expect("submit candidate after ordinary completion");
            let candidate_id = outcome.id();
            let outcome = outcome.wait().await.expect("candidate final outcome");
            match &outcome {
                TaskOutcome::Completed => {
                    immediate_completed += 1;
                    assert_eq!(candidate_starts.load(Ordering::Acquire), 1);
                }
                TaskOutcome::Rejected {
                    kind: RejectionKind::SlotBusy,
                    ..
                } => {
                    prior_slot_busy += 1;
                    assert_eq!(candidate_starts.load(Ordering::Acquire), 0);
                    if first_rejection.is_none() {
                        let snapshot = handle.controller_snapshot().await.unwrap();
                        let owner_alive = handle.is_alive(&owner_name).await;
                        first_rejection = Some(format!(
                            "iteration={iteration}; owner={owner_id:?}; candidate={candidate_id:?}; outcome={outcome:?}; starts=0; owner_alive={owner_alive}; observed_slot={:?}",
                            snapshot.slot(&slot),
                        ));
                    }
                }
                other => panic!("unexpected post-Completed outcome at {iteration}: {other:?}"),
            }
            wait_for_controller_slot_release(&handle, &slot).await;

            let released_starts = Arc::new(AtomicUsize::new(0));
            let starts = Arc::clone(&released_starts);
            let released_task = TaskFn::arc(move |_ctx| {
                starts.fetch_add(1, Ordering::AcqRel);
                async { Ok(()) }
            });
            let released_outcome = handle
                .submit(ControllerSpec::drop_if_running(TaskSpec::once(
                        format!("after-release-{iteration}"),
                        released_task,
                    ))
                    .with_slot(slot.clone())).watch().execute()
                .await
                .expect("single fresh Drop submission after observed controller release");
            let released_outcome = released_outcome.wait().await.expect("after-release outcome");
            assert!(
                matches!(released_outcome, TaskOutcome::Completed),
                "an observed free slot rejected fresh Drop work at {iteration}: {released_outcome:?}"
            );
            assert_eq!(released_starts.load(Ordering::Acquire), 1);
            assert_eq!(
                candidate_starts.load(Ordering::Acquire),
                usize::from(matches!(outcome, TaskOutcome::Completed)),
                "an earlier rejected candidate must never be replayed"
            );
            completed_after_release += 1;
            wait_for_controller_slot_release(&handle, &slot).await;
        }
        assert_eq!(immediate_completed + prior_slot_busy, PAIRS);
        assert_eq!(completed_after_release, PAIRS);
        handle.shutdown().await.expect("diagnostic shutdown");
        eprintln!(
            "post-Completed Drop audit ({runtime}): pairs={PAIRS}; immediate_completed={immediate_completed}; prior_slot_busy={prior_slot_busy}; completed_after_release={completed_after_release}; first_rejection={first_rejection:?}"
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_completed_drop_admission_audit_current_thread() {
    audit_drop_admission_after_ordinary_completion("current_thread").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_completed_drop_admission_audit_multi_thread() {
    audit_drop_admission_after_ordinary_completion("multi_thread").await;
}

#[tokio::test(flavor = "current_thread")]
async fn queue_full_rejects_with_controller_rejected_event() {
    let (handle, collector) = served_controller(ControllerConfig::default().with_max_slot_queue(1));
    with_timeout(10, async {
        let running = TaskSpec::restartable("r", make_coop());
        submit_running(&handle, ControllerSpec::queue(running).with_slot("s")).await;

        let p1 = TaskSpec::restartable("p1", make_coop());
        let p2 = TaskSpec::restartable("p2", make_coop());
        handle
            .submit(ControllerSpec::queue(p1).with_slot("s"))
            .execute()
            .await
            .unwrap();
        handle
            .submit(ControllerSpec::queue(p2).with_slot("s"))
            .execute()
            .await
            .unwrap();
        assert!(
            collector
                .wait_until(Duration::from_secs(3), |events| {
                    events.iter().any(|event| {
                        event.task.as_deref() == Some("s")
                            && event.kind == EventKind::ControllerRejected
                            && event.rejection_kind == Some(RejectionKind::QueueFull)
                            && event.outcome_kind == Some(TaskOutcomeKind::Rejected)
                    })
                })
                .await
        );
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn submit_try_intake_full_when_queue_capacity_saturated() {
    let (handle, _c) = served_controller(
        ControllerConfig::default().with_queue_capacity(NonZeroUsize::new(1).unwrap()),
    );
    with_timeout(10, async {
        let mut saw_full = false;
        for _ in 0..256 {
            let spec = TaskSpec::once("q", make_ok_once());
            if let Err(ControllerError::Full) = handle
                .submit(ControllerSpec::queue(spec).with_slot("q"))
                .try_intake()
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
                ControllerSpec::queue(TaskSpec::restartable("occupant-snap", make_coop()))
                    .with_slot("s"),
            )
            .execute()
            .await
            .expect("submit occupant ok");
        handle
            .submit(
                ControllerSpec::queue(TaskSpec::restartable("queued-snap", make_coop()))
                    .with_slot("s"),
            )
            .execute()
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

#[tokio::test(flavor = "current_thread")]
async fn natural_run_joins_controller_before_return() {
    let sup = Supervisor::builder(SupervisorConfig::default())
        .with_controller(ControllerConfig::default())
        .build();

    with_timeout(5, async {
        sup.run(vec![])
            .await
            .expect("empty run must finish cleanly");

        let handle = sup.serve().expect("runtime startup");
        let result = handle
            .submit(ControllerSpec::queue(TaskSpec::once(
                "after-natural-shutdown",
                make_ok_once(),
            )))
            .try_intake();
        assert_eq!(result, Err(ControllerError::Closed));
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_static_batch_keeps_controller_running() {
    let sup = Supervisor::builder(SupervisorConfig::default())
        .with_controller(ControllerConfig::default())
        .build();
    let first = TaskSpec::once("duplicate-static", make_ok_once());
    let second = TaskSpec::once("duplicate-static", make_ok_once());

    with_timeout(5, async {
        assert!(matches!(
            sup.run(vec![first, second]).await,
            Err(RuntimeError::TaskAlreadyExists { .. })
        ));

        let handle = sup.serve().expect("runtime startup");
        let waiter = handle
            .submit(ControllerSpec::queue(TaskSpec::once(
                "after-rejected-static-batch",
                make_ok_once(),
            )))
            .watch()
            .execute()
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
