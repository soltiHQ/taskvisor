//! Runtime-identity & dynamic task-management integration tests.

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use taskvisor::prelude::*;

fn served_with_collector(grace_secs: u64) -> (SupervisorHandle, Arc<EventCollector>) {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::builder(
        SupervisorConfig::default().with_grace(Duration::from_secs(grace_secs)),
    )
    .with_subscribers(subs)
    .build();
    (sup.serve(), collector)
}

async fn stale_id(handle: &SupervisorHandle) -> TaskId {
    let id = handle
        .add(TaskSpec::restartable(make_coop("throwaway")))
        .await
        .expect("add ok");
    assert!(handle.cancel(id).await.expect("cancel ok"));
    assert!(
        poll_until(Duration::from_secs(2), || async {
            !handle.list().await.iter().any(|(i, _)| *i == id)
        })
        .await
    );
    id
}

#[tokio::test(flavor = "current_thread")]
async fn add_returns_taskid_and_task_runs() {
    let (handle, collector) = served_with_collector(5);
    with_timeout(10, async {
        let id = handle
            .add(TaskSpec::restartable(make_coop("worker")))
            .await
            .expect("add ok");
        assert!(
            poll_until(Duration::from_secs(2), || async {
                handle.is_alive("worker").await
                    && collector.by_id(id).iter().any(|e| {
                        e.kind == EventKind::TaskStarting && e.task.as_deref() == Some("worker")
                    })
            })
            .await
        );
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn add_confirms_registration_and_returns_id() {
    let (handle, _c) = served_with_collector(5);
    with_timeout(10, async {
        let id = handle
            .add(TaskSpec::restartable(make_coop("w")))
            .await
            .expect("add ok");
        assert!(
            poll_until(Duration::from_secs(1), || async {
                handle.is_alive("w").await
            })
            .await
        );
        let list = handle.list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, id);
        assert_eq!(&*list[0].1, "w");
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_add_returns_error_and_only_first_runs() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (handle, _collector) = served_with_collector(5);
    with_timeout(10, async {
        let id1 = handle
            .add(TaskSpec::restartable(make_coop("dup")))
            .await
            .expect("add1");

        let rejected_runs = Arc::new(AtomicUsize::new(0));
        let task_runs = Arc::clone(&rejected_runs);
        let rejected: TaskRef = TaskFn::arc("dup", move |_ctx: TaskContext| {
            task_runs.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        });
        let error = handle
            .add(TaskSpec::once(rejected))
            .await
            .expect_err("duplicate add must fail");
        assert!(matches!(
            error,
            RuntimeError::TaskAlreadyExists { ref name, .. } if name.as_ref() == "dup"
        ));
        assert_eq!(rejected_runs.load(Ordering::SeqCst), 0);

        let list = handle.list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, id1);
        assert!(handle.is_alive("dup").await);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn add_duplicate_name_returns_already_exists() {
    let (handle, _c) = served_with_collector(5);
    with_timeout(10, async {
        let _ = handle
            .add(TaskSpec::restartable(make_coop("dup")))
            .await
            .expect("first add ok");
        let err = handle
            .add(TaskSpec::restartable(make_coop("dup")))
            .await
            .expect_err("second add must fail");
        match err {
            RuntimeError::TaskAlreadyExists { name, .. } => assert_eq!(&*name, "dup"),
            other => panic!("expected TaskAlreadyExists, got {other:?}"),
        }
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn fast_task_registration_has_no_library_timeout() {
    let (handle, _c) = served_with_collector(5);
    with_timeout(10, async {
        let id = handle
            .add(TaskSpec::once(make_ok_once("fast-registration")))
            .await
            .expect("registry reply must confirm even a fast task");
        assert!(id.get() > 0);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn remove_by_id_removes_only_that_id() {
    let (handle, collector) = served_with_collector(5);
    with_timeout(10, async {
        let id_a = handle
            .add(TaskSpec::restartable(make_coop("a")))
            .await
            .expect("add a");
        let id_b = handle
            .add(TaskSpec::restartable(make_coop("b")))
            .await
            .expect("add b");

        assert!(handle.remove(id_a).await.expect("remove a"));

        assert!(
            poll_until(Duration::from_secs(2), || async {
                let list = handle.list().await;
                list.len() == 1
                    && list[0].0 == id_b
                    && !handle.is_alive("a").await
                    && collector
                        .by_id(id_a)
                        .iter()
                        .any(|event| event.kind == EventKind::TaskRemoved)
            })
            .await
        );
        assert!(handle.is_alive("b").await);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn remove_unknown_id_is_noop_without_terminal_event() {
    let (handle, collector) = served_with_collector(5);
    with_timeout(10, async {
        let id_keep = handle
            .add(TaskSpec::restartable(make_coop("keep")))
            .await
            .expect("add keep");
        let stale = stale_id(&handle).await;

        assert!(!handle.remove(stale).await.expect("remove stale ok"));
        assert!(!handle.try_remove(stale).await.expect("try_remove stale ok"));
        handle
            .add(TaskSpec::restartable(make_coop("remove-unknown-barrier")))
            .await
            .expect("later add confirms that the unknown remove was processed");
        assert!(
            collector
                .by_id(stale)
                .iter()
                .all(|event| event.kind != EventKind::TaskRemoved),
            "an unknown id has no terminal transition to report"
        );
        let list = handle.list().await;
        assert!(list.iter().any(|(i, l)| *i == id_keep && &**l == "keep"));
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn remove_by_label_returns_true_then_false() {
    let (handle, _c) = served_with_collector(5);
    with_timeout(10, async {
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let task_started = Arc::clone(&started);
        let task_release = Arc::clone(&release);
        let task: TaskRef = TaskFn::arc("svc", move |_ctx: TaskContext| {
            let started = Arc::clone(&task_started);
            let release = Arc::clone(&task_release);
            async move {
                started.notify_one();
                release.notified().await;
                Ok(())
            }
        });
        let id = handle
            .add(TaskSpec::restartable(task))
            .await
            .expect("add svc");
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .expect("svc must start before removal");

        assert!(handle.remove_by_label("svc").await.expect("remove1"));
        assert!(!handle.try_remove_by_label("svc").await.expect("remove2"));
        assert_eq!(handle.list().await, vec![(id, Arc::from("svc"))]);

        release.notify_one();
        assert!(
            poll_until(Duration::from_secs(2), || async {
                !handle.list().await.iter().any(|(_, l)| &**l == "svc")
            })
            .await
        );
        assert!(
            !handle
                .remove_by_label("never-existed")
                .await
                .expect("remove unknown label")
        );
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_by_id_true_then_false_on_double_cancel() {
    let (handle, _c) = served_with_collector(5);
    with_timeout(10, async {
        let id = handle
            .add(TaskSpec::restartable(make_coop("c")))
            .await
            .expect("add c");
        assert!(handle.cancel(id).await.expect("cancel1"));
        assert!(!handle.cancel(id).await.expect("cancel2"));
        assert!(!handle.is_alive("c").await);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_unknown_id_returns_false() {
    let (handle, _c) = served_with_collector(5);
    with_timeout(10, async {
        let stale = stale_id(&handle).await;
        assert!(!handle.cancel(stale).await.expect("cancel stale"));
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_by_label_true_then_false() {
    let (handle, _c) = served_with_collector(5);
    with_timeout(10, async {
        let _ = handle
            .add(TaskSpec::restartable(make_coop("lbl")))
            .await
            .expect("add lbl");
        assert!(handle.cancel_by_label("lbl").await.expect("c1"));
        assert!(
            poll_until(Duration::from_secs(2), || async {
                !handle.list().await.iter().any(|(_, l)| &**l == "lbl")
            })
            .await
        );
        assert!(!handle.cancel_by_label("lbl").await.expect("c2"));
        assert!(!handle.cancel_by_label("ghost").await.expect("c3"));
        assert!(!handle.is_alive("lbl").await);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_with_timeout_true_for_cooperative_task() {
    let (handle, _c) = served_with_collector(5);
    with_timeout(10, async {
        let id = handle
            .add(TaskSpec::restartable(make_coop("coop")))
            .await
            .expect("add coop");
        assert!(
            handle
                .cancel_with_timeout(id, Duration::from_secs(2))
                .await
                .expect("cancel_with_timeout")
        );
        assert!(!handle.is_alive("coop").await);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_by_label_with_timeout_variants_are_public_contracts() {
    let (handle, _c) = served_with_collector(5);
    with_timeout(10, async {
        let _ = handle
            .add(TaskSpec::restartable(make_coop("label-timeout")))
            .await
            .expect("add label-timeout");
        assert!(
            handle
                .cancel_by_label_with_timeout("label-timeout", Duration::from_secs(2))
                .await
                .expect("cancel_by_label_with_timeout")
        );

        let _ = handle
            .add(TaskSpec::restartable(make_coop("try-label-timeout")))
            .await
            .expect("add try-label-timeout");
        assert!(
            handle
                .try_cancel_by_label_with_timeout("try-label-timeout", Duration::from_secs(2),)
                .await
                .expect("try_cancel_by_label_with_timeout")
        );

        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_timeout_does_not_stop_shared_removal() {
    let (handle, _c) = served_with_collector(5);
    with_timeout(10, async {
        let started = Arc::new(tokio::sync::Notify::new());
        let cancellation_seen = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let started_by_task = Arc::clone(&started);
        let seen_by_task = Arc::clone(&cancellation_seen);
        let release_by_task = Arc::clone(&release);
        let task = TaskFn::arc("timeout-shared", move |ctx: TaskContext| {
            let started = Arc::clone(&started_by_task);
            let seen = Arc::clone(&seen_by_task);
            let release = Arc::clone(&release_by_task);
            async move {
                started.notify_one();
                ctx.cancelled().await;
                seen.notify_one();
                release.notified().await;
                Ok(())
            }
        });
        let id = handle
            .add(TaskSpec::restartable(task))
            .await
            .expect("add timeout-shared");
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .expect("task must start before cancellation");

        match handle.cancel_with_timeout(id, Duration::ZERO).await {
            Err(RuntimeError::TaskTerminationTimeout {
                id: timed_id,
                timeout,
                ..
            }) => {
                assert_eq!(timed_id, id);
                assert_eq!(timeout, Duration::ZERO);
            }
            other => panic!(
                "expected TaskTerminationTimeout while terminal completion is blocked, got {other:?}"
            ),
        }
        tokio::time::timeout(Duration::from_secs(2), cancellation_seen.notified())
            .await
            .expect("timed-out caller must leave cancellation running");
        assert_eq!(handle.list().await, vec![(id, Arc::from("timeout-shared"))]);

        let mut joined_cancel = Box::pin(handle.cancel(id));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut joined_cancel)
                .await
                .is_err(),
            "a later cancel must join the same blocked terminal completion"
        );

        release.notify_one();
        assert!(
            !tokio::time::timeout(Duration::from_secs(2), joined_cancel)
                .await
                .expect("joined cancel must finish after release")
                .expect("joined cancel must not fail"),
            "the caller that joins an existing removal returns false"
        );
        assert!(handle.list().await.is_empty());

        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn individually_removed_stuck_task_is_force_aborted_after_grace() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup =
        Supervisor::builder(SupervisorConfig::default().with_grace(Duration::from_millis(300)))
            .with_subscribers(subs)
            .build();
    let handle = sup.serve();

    with_timeout(10, async {
        let task = TaskFn::arc("stuck-runner", |_ctx: TaskContext| async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        });
        let id = handle
            .add(TaskSpec::restartable(task))
            .await
            .expect("add stuck-runner");

        assert!(handle.remove(id).await.expect("remove"));

        assert!(
            poll_until(Duration::from_secs(3), || async {
                collector.by_id(id).iter().any(|e| {
                    e.kind == EventKind::TaskRemoved
                        && e.reason.as_deref() == Some("force_terminated_after_grace")
                })
            })
            .await,
            "stuck task must be force-aborted after grace, not leaked"
        );
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn list_reflects_registered_set_sorted_by_id() {
    let (handle, _c) = served_with_collector(5);
    with_timeout(10, async {
        let id_x = handle
            .add(TaskSpec::restartable(make_coop("x")))
            .await
            .unwrap();
        let id_y = handle
            .add(TaskSpec::restartable(make_coop("y")))
            .await
            .unwrap();
        let id_z = handle
            .add(TaskSpec::restartable(make_coop("z")))
            .await
            .unwrap();

        let list = handle.list().await;
        assert_eq!(list.len(), 3);
        let ids: Vec<TaskId> = list.iter().map(|(i, _)| *i).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "list must be sorted ascending by id");
        let labels: HashSet<&str> = list.iter().map(|(_, l)| &**l).collect();
        assert_eq!(labels, HashSet::from(["x", "y", "z"]));
        assert!(ids.contains(&id_x) && ids.contains(&id_y) && ids.contains(&id_z));

        assert!(handle.remove(id_y).await.expect("remove y"));
        assert!(
            poll_until(Duration::from_secs(2), || async {
                let l = handle.list().await;
                l.len() == 2 && !l.iter().any(|(i, _)| *i == id_y)
            })
            .await
        );
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn snapshot_and_is_alive_track_alive_set() {
    let (handle, _c) = served_with_collector(5);
    with_timeout(10, async {
        let _ = handle
            .add(TaskSpec::restartable(make_coop("live")))
            .await
            .unwrap();
        let _ = handle
            .add(TaskSpec::once(make_ok_once("oneshot")))
            .await
            .unwrap();

        assert!(
            poll_until(Duration::from_secs(2), || async {
                handle.is_alive("live").await
            })
            .await
        );
        assert!(handle.alive_snapshot().await.iter().any(|n| &**n == "live"));

        assert!(
            poll_until(Duration::from_secs(2), || async {
                !handle.is_alive("oneshot").await
                    && !handle
                        .alive_snapshot()
                        .await
                        .iter()
                        .any(|n| &**n == "oneshot")
                    && !handle.list().await.iter().any(|(_, l)| &**l == "oneshot")
            })
            .await
        );

        let snap = handle.alive_snapshot().await;
        let mut sorted = snap.clone();
        sorted.sort();
        assert_eq!(snap, sorted);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn is_alive_false_for_unknown_label() {
    let (handle, _c) = served_with_collector(5);
    with_timeout(5, async {
        assert!(!handle.is_alive("nope").await);
        assert!(handle.alive_snapshot().await.is_empty());
        assert!(handle.list().await.is_empty());
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn events_carry_correct_id_across_full_lifecycle() {
    let (handle, collector) = served_with_collector(5);
    with_timeout(10, async {
        let id = handle
            .add(TaskSpec::restartable(make_coop("life")))
            .await
            .unwrap();
        assert!(handle.cancel(id).await.expect("cancel"));

        assert!(
            poll_until(Duration::from_secs(2), || async {
                collector
                    .by_id(id)
                    .iter()
                    .any(|e| e.kind == EventKind::TaskRemoved)
            })
            .await
        );

        let by_id = collector.by_id(id);
        assert!(by_id.iter().any(|e| e.kind == EventKind::TaskStarting));
        assert!(by_id.iter().any(|e| e.kind == EventKind::TaskRemoved));
        for e in collector.by_label("life") {
            if let Some(eid) = e.id {
                assert_eq!(eid, id, "event {:?} carried a foreign id", e.kind);
            }
        }
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn re_add_same_label_after_removal_succeeds_with_new_id() {
    let (handle, _c) = served_with_collector(5);
    with_timeout(10, async {
        let id1 = handle
            .add(TaskSpec::restartable(make_coop("reuse")))
            .await
            .unwrap();
        assert!(handle.cancel(id1).await.expect("cancel"));
        assert!(
            poll_until(Duration::from_secs(2), || async {
                !handle.list().await.iter().any(|(_, l)| &**l == "reuse")
            })
            .await
        );
        let id2 = handle
            .add(TaskSpec::restartable(make_coop("reuse")))
            .await
            .unwrap();
        assert_ne!(id1, id2, "re-added label must get a fresh id");
        let list = handle.list().await;
        assert!(list.iter().any(|(i, l)| *i == id2 && &**l == "reuse"));
        assert!(!list.iter().any(|(i, _)| *i == id1));
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn add_after_shutdown_returns_shutting_down() {
    let (handle, _c) = served_with_collector(5);
    let h2 = handle.clone();
    with_timeout(10, async {
        with_timeout(5, handle.shutdown())
            .await
            .expect("shutdown ok");
        assert!(matches!(
            h2.add(TaskSpec::once(make_ok_once("late"))).await,
            Err(RuntimeError::ShuttingDown)
        ));
    })
    .await;
}
