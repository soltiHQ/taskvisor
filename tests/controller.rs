//! Controller admission-policy integration tests (requires feature `controller`).

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use taskvisor::prelude::*;
use taskvisor::{ControllerConfig, ControllerError, ControllerSpec};

fn served_controller(cfg: ControllerConfig) -> (SupervisorHandle, Arc<EventCollector>) {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::builder(SupervisorConfig {
        grace: Duration::from_secs(5),
        ..Default::default()
    })
    .with_subscribers(subs)
    .with_controller(cfg)
    .build();
    (sup.serve(), collector)
}

fn logging_once(name: &str, log: Arc<Mutex<Vec<String>>>) -> TaskRef {
    let n = name.to_string();
    TaskFn::arc(name, move |_ctx: CancellationToken| {
        let log = log.clone();
        let n = n.clone();
        async move {
            log.lock().unwrap().push(n);
            Ok(())
        }
    })
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_does_not_start_queued_tasks() {
    let (handle, collector) = served_controller(ControllerConfig::default());

    with_timeout(10, async {
        // Occupy the slot with a long-running cooperative task.
        handle
            .submit(ControllerSpec::queue(
                TaskSpec::restartable(make_coop("occupant")).with_slot("s"),
            ))
            .await
            .expect("first submit ok");
        assert!(
            poll_until(Duration::from_secs(2), || async {
                handle.is_alive("occupant").await
            })
            .await,
            "occupant must be running before queueing the next task"
        );

        // Queue a successor behind it.
        handle
            .submit(ControllerSpec::queue(
                TaskSpec::restartable(make_coop("queued")).with_slot("s"),
            ))
            .await
            .expect("second submit ok");

        handle.shutdown().await.expect("shutdown ok");
    })
    .await;

    // Draining the occupant publishes TaskRemoved; the controller must NOT react
    // by starting the queued task mid-shutdown (it would be force-aborted with no grace).
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
        let spec = TaskSpec::restartable(make_coop("runner-7")).with_slot("web");
        handle.submit(ControllerSpec::queue(spec)).await.unwrap();

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
            if matches!(
                e.kind,
                EventKind::ControllerSubmitted | EventKind::ControllerSlotTransition
            ) {
                assert_eq!(e.id, None, "controller events must not carry a TaskId");
            }
        }
        assert!(
            collector
                .by_label("runner-7")
                .iter()
                .any(|e| { e.kind == EventKind::TaskStarting && e.id.is_some() })
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
            let spec = TaskSpec::once(logging_once(name, log.clone())).with_slot("q");
            handle.submit(ControllerSpec::queue(spec)).await.unwrap();
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
        let run1 = TaskSpec::restartable(make_coop("run-1")).with_slot("s");
        handle.submit(ControllerSpec::replace(run1)).await.unwrap();
        assert!(
            poll_until(Duration::from_secs(3), || async {
                handle.is_alive("run-1").await
            })
            .await
        );

        let run2 = TaskSpec::restartable(make_coop("run-2")).with_slot("s");
        handle.submit(ControllerSpec::replace(run2)).await.unwrap();

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
        let first = TaskSpec::restartable(make_coop("first")).with_slot("s");
        handle
            .submit(ControllerSpec::drop_if_running(first))
            .await
            .unwrap();
        assert!(
            poll_until(Duration::from_secs(3), || async {
                handle.is_alive("first").await
            })
            .await
        );

        let second = TaskSpec::restartable(make_coop("second")).with_slot("s");
        handle
            .submit(ControllerSpec::drop_if_running(second))
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
        let solo = TaskSpec::restartable(make_coop("solo")).with_slot("s");
        handle
            .submit(ControllerSpec::drop_if_running(solo))
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
        let w1 = TaskSpec::restartable(make_coop("w1")).with_slot("s1");
        let w2 = TaskSpec::restartable(make_coop("w2")).with_slot("s2");
        handle.submit(ControllerSpec::queue(w1)).await.unwrap();
        handle.submit(ControllerSpec::queue(w2)).await.unwrap();

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
async fn replace_into_idle_slot_behaves_as_plain_admit() {
    let (handle, collector) = served_controller(ControllerConfig::default());
    with_timeout(10, async {
        let x = TaskSpec::restartable(make_coop("x")).with_slot("s");
        handle.submit(ControllerSpec::replace(x)).await.unwrap();
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
        let first = TaskSpec::once(make_ok_once("first")).with_slot("s");
        handle.submit(ControllerSpec::queue(first)).await.unwrap();
        assert!(
            poll_until(Duration::from_secs(3), || async {
                !handle.is_alive("first").await
            })
            .await
        );
        assert!(
            poll_until(Duration::from_secs(3), || async {
                if !handle.is_alive("second").await {
                    let second = TaskSpec::restartable(make_coop("second")).with_slot("s");
                    let _ = handle.submit(ControllerSpec::drop_if_running(second)).await;
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
    let (handle, collector) = served_controller(ControllerConfig {
        queue_capacity: 1024,
        max_slot_queue: 1,
    });
    with_timeout(10, async {
        let running = TaskSpec::restartable(make_coop("r")).with_slot("s");
        handle.submit(ControllerSpec::queue(running)).await.unwrap();
        assert!(
            poll_until(Duration::from_secs(3), || async {
                handle.is_alive("r").await
            })
            .await
        );

        let p1 = TaskSpec::restartable(make_coop("p1")).with_slot("s");
        let p2 = TaskSpec::restartable(make_coop("p2")).with_slot("s");
        handle.submit(ControllerSpec::queue(p1)).await.unwrap();
        handle.submit(ControllerSpec::queue(p2)).await.unwrap();
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
    let (handle, _c) = served_controller(ControllerConfig {
        queue_capacity: 1,
        max_slot_queue: 100,
    });
    with_timeout(10, async {
        let mut saw_full = false;
        for i in 0..256u32 {
            let spec = TaskSpec::once(make_ok_once("q")).with_slot("q");
            let _ = i;
            if let Err(ControllerError::Full) = handle.try_submit(ControllerSpec::queue(spec)) {
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
