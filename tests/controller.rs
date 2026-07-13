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
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::builder(SupervisorConfig::default().with_grace(Duration::from_secs(5)))
        .with_subscribers(subs)
        .with_controller(cfg)
        .build();
    (sup.serve(), collector)
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

#[tokio::test(flavor = "current_thread")]
async fn submit_and_watch_resolves_completed_for_admitted_task() {
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

        handle.shutdown().await.expect("shutdown ok");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn submit_and_watch_resolves_rejected_on_drop_if_running() {
    let (handle, _collector) = served_controller(ControllerConfig::default());

    with_timeout(10, async {
        handle
            .submit(
                ControllerSpec::queue(TaskSpec::restartable(make_coop("occupant-w")))
                    .with_slot("s"),
            )
            .await
            .expect("first submit ok");
        assert!(
            poll_until(Duration::from_secs(2), || async {
                handle.is_alive("occupant-w").await
            })
            .await
        );

        let (_id, waiter) = handle
            .submit_and_watch(
                ControllerSpec::drop_if_running(TaskSpec::restartable(make_coop("dropped-w")))
                    .with_slot("s"),
            )
            .await
            .expect("submit_and_watch accepted into channel");

        match waiter.wait().await.expect("waiter errored") {
            TaskOutcome::Rejected { reason } => {
                assert!(
                    reason.contains("dropped"),
                    "rejection reason must explain why: {reason}"
                );
            }
            other => panic!("expected Rejected, got {other:?}"),
        }

        handle.shutdown().await.expect("shutdown ok");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_immediately_removes_a_watched_queued_submission() {
    let (handle, _collector) = served_controller(ControllerConfig::default());

    with_timeout(10, async {
        handle
            .submit(
                ControllerSpec::queue(TaskSpec::restartable(make_coop("occupant-rm")))
                    .with_slot("s"),
            )
            .await
            .expect("first submit ok");
        assert!(
            poll_until(Duration::from_secs(2), || async {
                handle.is_alive("occupant-rm").await
            })
            .await
        );

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

        match waiter.wait().await.expect("waiter errored") {
            TaskOutcome::Rejected { reason } => {
                assert_eq!(&*reason, "removed_from_queue");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }

        handle.shutdown().await.expect("shutdown ok");
    })
    .await;
}

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
async fn submit_returns_task_id_carried_by_events() {
    let (handle, collector) = served_controller(ControllerConfig::default());

    with_timeout(10, async {
        let id = handle
            .submit(
                ControllerSpec::queue(TaskSpec::restartable(make_coop("id-task"))).with_slot("s"),
            )
            .await
            .expect("submit ok");

        assert!(
            poll_until(Duration::from_secs(2), || async {
                collector
                    .find_all(EventKind::TaskAdded)
                    .iter()
                    .any(|e| e.id == Some(id))
            })
            .await,
            "TaskAdded must carry the id returned by submit()"
        );
        assert!(
            collector
                .find_all(EventKind::ControllerSubmitted)
                .iter()
                .any(|e| e.id == Some(id)),
            "ControllerSubmitted must carry the submitted id"
        );

        handle.shutdown().await.expect("shutdown ok");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_submission_event_carries_its_id() {
    let (handle, collector) = served_controller(ControllerConfig::default());

    with_timeout(10, async {
        handle
            .submit(
                ControllerSpec::queue(TaskSpec::restartable(make_coop("occupant-rej")))
                    .with_slot("s"),
            )
            .await
            .expect("first submit ok");
        assert!(
            poll_until(Duration::from_secs(2), || async {
                handle.is_alive("occupant-rej").await
            })
            .await
        );

        let rejected_id = handle
            .submit(
                ControllerSpec::drop_if_running(TaskSpec::restartable(make_coop("dropped")))
                    .with_slot("s"),
            )
            .await
            .expect("submit accepted into channel");

        assert!(
            poll_until(Duration::from_secs(2), || async {
                collector
                    .find_all(EventKind::ControllerRejected)
                    .iter()
                    .any(|e| e.id == Some(rejected_id))
            })
            .await,
            "ControllerRejected must carry the id of the dropped submission"
        );

        handle.shutdown().await.expect("shutdown ok");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn remove_of_queued_submission_purges_it_before_start() {
    let (handle, collector) = served_controller(ControllerConfig::default());

    with_timeout(10, async {
        handle
            .submit(
                ControllerSpec::queue(TaskSpec::restartable(make_coop("occupant-q")))
                    .with_slot("s"),
            )
            .await
            .expect("first submit ok");
        assert!(
            poll_until(Duration::from_secs(2), || async {
                handle.is_alive("occupant-q").await
            })
            .await
        );

        let victim_id = handle
            .submit(
                ControllerSpec::queue(TaskSpec::restartable(make_coop("queued-victim")))
                    .with_slot("s"),
            )
            .await
            .expect("second submit ok");

        assert!(
            poll_until(Duration::from_secs(2), || async {
                collector
                    .find_all(EventKind::ControllerSubmitted)
                    .iter()
                    .any(|e| e.id == Some(victim_id))
            })
            .await,
            "queued submission must be confirmed before removal"
        );

        assert!(
            handle.remove(victim_id).await.expect("remove accepted"),
            "remove must claim a queued controller submission"
        );
        assert!(
            poll_until(Duration::from_secs(2), || async {
                collector
                    .find_all(EventKind::ControllerRejected)
                    .iter()
                    .any(|e| {
                        e.id == Some(victim_id) && e.reason.as_deref() == Some("removed_from_queue")
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
        tokio::time::sleep(Duration::from_millis(100)).await;
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

#[tokio::test(flavor = "current_thread")]
async fn shutdown_does_not_start_queued_tasks() {
    let (handle, collector) = served_controller(ControllerConfig::default());

    with_timeout(10, async {
        handle
            .submit(
                ControllerSpec::queue(TaskSpec::restartable(make_coop("occupant"))).with_slot("s"),
            )
            .await
            .expect("first submit ok");
        assert!(
            poll_until(Duration::from_secs(2), || async {
                handle.is_alive("occupant").await
            })
            .await,
            "occupant must be running before queueing the next task"
        );

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
async fn submit_without_controller_via_new_returns_not_configured() {
    let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    let handle = sup.serve();
    let spec = TaskSpec::once(make_ok_once("t"));
    with_timeout(5, async {
        assert_eq!(
            handle.submit(ControllerSpec::queue(spec.clone())).await,
            Err(ControllerError::NotConfigured)
        );
        assert_eq!(
            handle.try_submit(ControllerSpec::queue(spec)),
            Err(ControllerError::NotConfigured)
        );
        assert!(handle.list().await.is_empty());
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn submit_without_controller_via_plain_builder_returns_not_configured() {
    let sup = Supervisor::builder(SupervisorConfig::default())
        .with_subscribers(vec![])
        .build();
    let handle = sup.serve();
    with_timeout(5, async {
        assert_eq!(
            handle.try_submit(ControllerSpec::drop_if_running(TaskSpec::once(
                make_ok_once("t")
            ))),
            Err(ControllerError::NotConfigured)
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn idle_submit_admits_emits_submitted_then_running_transition() {
    let (handle, collector) = served_controller(ControllerConfig::default());
    with_timeout(10, async {
        let spec = TaskSpec::restartable(make_coop("runner-7"));
        let id = handle
            .submit(ControllerSpec::queue(spec).with_slot("web"))
            .await
            .unwrap();

        assert!(
            poll_until(Duration::from_secs(3), || async {
                handle.is_alive("runner-7").await
                    && collector.by_label("web").iter().any(|e| {
                        e.kind == EventKind::ControllerSlotTransition
                            && e.reason.as_deref() == Some("admitting→running")
                    })
            })
            .await
        );

        assert!(collector.by_label("web").iter().any(|e| {
            e.kind == EventKind::ControllerSubmitted
                && e.reason
                    .as_deref()
                    .is_some_and(|r| r.contains("status=admitting"))
        }));
        for e in collector.by_label("web") {
            if e.kind == EventKind::ControllerSubmitted {
                assert_eq!(
                    e.id,
                    Some(id),
                    "ControllerSubmitted must carry the submission TaskId"
                );
            }
        }
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
        handle
            .submit(ControllerSpec::replace(run1).with_slot("s"))
            .await
            .unwrap();
        assert!(
            poll_until(Duration::from_secs(3), || async {
                handle.is_alive("run-1").await
            })
            .await
        );

        let run2 = TaskSpec::restartable(make_coop("run-2"));
        handle
            .submit(ControllerSpec::replace(run2).with_slot("s"))
            .await
            .unwrap();

        assert!(
            poll_until(Duration::from_secs(4), || async {
                let snap = handle.snapshot().await;
                snap.iter().any(|n| &**n == "run-2") && !snap.iter().any(|n| &**n == "run-1")
            })
            .await,
            "latest-wins: run-2 alive, run-1 gone"
        );

        assert!(collector.by_label("s").iter().any(|e| {
            e.kind == EventKind::ControllerSlotTransition
                && e.reason.as_deref() == Some("running→terminating (replace)")
        }));
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn drop_if_running_rejects_while_busy_silently() {
    let (handle, collector) = served_controller(ControllerConfig::default());
    with_timeout(10, async {
        let first = TaskSpec::restartable(make_coop("first"));
        handle
            .submit(ControllerSpec::drop_if_running(first).with_slot("s"))
            .await
            .unwrap();
        assert!(
            poll_until(Duration::from_secs(3), || async {
                handle.is_alive("first").await
            })
            .await
        );

        let second = TaskSpec::restartable(make_coop("second"));
        handle
            .submit(ControllerSpec::drop_if_running(second).with_slot("s"))
            .await
            .unwrap();

        assert!(
            poll_until(Duration::from_secs(3), || async {
                collector.by_label("s").iter().any(|e| {
                    e.kind == EventKind::ControllerRejected
                        && e.reason
                            .as_deref()
                            .is_some_and(|r| r.contains("dropped: slot busy"))
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
        handle
            .submit(ControllerSpec::drop_if_running(solo).with_slot("s"))
            .await
            .unwrap();
        assert!(
            poll_until(Duration::from_secs(3), || async {
                handle.is_alive("solo").await
            })
            .await
        );

        assert!(
            collector
                .by_label("s")
                .iter()
                .any(|e| e.kind == EventKind::ControllerSubmitted)
        );
        assert_eq!(collector.count(EventKind::ControllerRejected), 0);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn same_name_distinct_slots_both_admitted() {
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
        handle
            .submit(ControllerSpec::queue(first).with_slot("s1"))
            .await
            .expect("first submit accepted");

        assert!(
            poll_until(Duration::from_secs(4), || async {
                handle.is_alive("dup").await
            })
            .await,
            "first task must be registered before the duplicate is submitted"
        );

        let (_id, waiter) = handle
            .submit_and_watch(
                ControllerSpec::queue(TaskSpec::restartable(make_coop("dup"))).with_slot("s2"),
            )
            .await
            .expect("second submit_and_watch accepted into channel");

        let result = waiter.wait().await;
        assert!(
            matches!(result, Ok(TaskOutcome::Rejected { .. })),
            "duplicate task name in a distinct slot must resolve Ok(Rejected), got {result:?}"
        );

        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn replace_into_idle_slot_behaves_as_plain_admit() {
    let (handle, collector) = served_controller(ControllerConfig::default());
    with_timeout(10, async {
        let x = TaskSpec::restartable(make_coop("x"));
        handle
            .submit(ControllerSpec::replace(x).with_slot("s"))
            .await
            .unwrap();
        assert!(
            poll_until(Duration::from_secs(3), || async {
                handle.is_alive("x").await
            })
            .await
        );

        assert!(
            poll_until(Duration::from_secs(2), || async {
                collector.by_label("s").iter().any(|e| {
                    e.kind == EventKind::ControllerSlotTransition
                        && e.reason.as_deref() == Some("admitting→running")
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
        let first = TaskSpec::once(make_ok_once("first"));
        handle
            .submit(ControllerSpec::queue(first).with_slot("s"))
            .await
            .unwrap();
        assert!(
            poll_until(Duration::from_secs(3), || async {
                !handle.is_alive("first").await
            })
            .await
        );
        assert!(
            poll_until(Duration::from_secs(3), || async {
                if !handle.is_alive("second").await {
                    let second = TaskSpec::restartable(make_coop("second"));
                    let _ = handle
                        .submit(ControllerSpec::drop_if_running(second).with_slot("s"))
                        .await;
                }
                handle.is_alive("second").await
            })
            .await,
            "freed slot must eventually admit a DropIfRunning submission"
        );
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn queue_full_rejects_with_controller_rejected_event() {
    let (handle, collector) = served_controller(ControllerConfig::default().with_max_slot_queue(1));
    with_timeout(10, async {
        let running = TaskSpec::restartable(make_coop("r"));
        handle
            .submit(ControllerSpec::queue(running).with_slot("s"))
            .await
            .unwrap();
        assert!(
            poll_until(Duration::from_secs(3), || async {
                handle.is_alive("r").await
            })
            .await
        );

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
            poll_until(Duration::from_secs(3), || async {
                collector.by_label("s").iter().any(|e| {
                    e.kind == EventKind::ControllerRejected
                        && e.reason
                            .as_deref()
                            .is_some_and(|r| r.contains("queue_full"))
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
        for i in 0..256u32 {
            let spec = TaskSpec::once(make_ok_once("q"));
            let _ = i;
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
        handle
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

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut observed = false;
        while tokio::time::Instant::now() < deadline {
            if let Some(snap) = handle.controller_snapshot().await
                && let Some(view) = snap.slot("s")
                && view.status == SlotStatusKind::Running
                && view.queue_depth == 1
                && snap.running_count() == 1
                && snap.total_queued() == 1
            {
                observed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
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
