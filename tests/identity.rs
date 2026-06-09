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
    let sup = Supervisor::builder(SupervisorConfig {
        grace: Duration::from_secs(grace_secs),
        ..Default::default()
    })
    .with_subscribers(subs)
    .build();
    (sup.serve(), collector)
}

async fn stale_id(handle: &SupervisorHandle) -> TaskId {
    let id = handle
        .add_and_wait(
            TaskSpec::restartable(make_coop("throwaway")),
            Duration::from_secs(2),
        )
        .await
        .expect("add_and_wait ok");
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
async fn add_and_wait_confirms_running_returns_id() {
    let (handle, _c) = served_with_collector(5);
    with_timeout(10, async {
        let id = handle
            .add_and_wait(
                TaskSpec::restartable(make_coop("w")),
                Duration::from_secs(2),
            )
            .await
            .expect("add_and_wait ok");
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
async fn two_tasks_same_name_get_different_ids_only_first_runs() {
    let (handle, collector) = served_with_collector(5);
    with_timeout(10, async {
        let id1 = handle
            .add(TaskSpec::restartable(make_coop("dup")))
            .expect("add1");
        let id2 = handle
            .add(TaskSpec::restartable(make_coop("dup")))
            .expect("add2");
        assert_ne!(id1, id2, "same name must still mint distinct ids");

        assert!(
            poll_until(Duration::from_secs(2), || async {
                collector
                    .by_id(id1)
                    .iter()
                    .any(|e| e.kind == EventKind::TaskAdded)
                    && collector
                        .by_id(id2)
                        .iter()
                        .any(|e| e.kind == EventKind::TaskAddFailed)
            })
            .await
        );

        let failed = collector
            .by_id(id2)
            .into_iter()
            .find(|e| e.kind == EventKind::TaskAddFailed)
            .unwrap();
        assert!(failed.reason.as_deref().unwrap().contains("already_exists"));

        let list = handle.list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, id1);
        assert!(handle.is_alive("dup").await);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn add_and_wait_duplicate_name_returns_already_exists() {
    let (handle, _c) = served_with_collector(5);
    with_timeout(10, async {
        let _ = handle
            .add_and_wait(
                TaskSpec::restartable(make_coop("dup")),
                Duration::from_secs(2),
            )
            .await
            .expect("first add ok");
        let err = handle
            .add_and_wait(
                TaskSpec::restartable(make_coop("dup")),
                Duration::from_secs(2),
            )
            .await
            .expect_err("second add must fail");
        match err {
            RuntimeError::TaskAlreadyExists { name } => assert_eq!(&*name, "dup"),
            other => panic!("expected TaskAlreadyExists, got {other:?}"),
        }
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn add_and_wait_tiny_timeout_accepts_ok_or_addtimeout() {
    let (handle, _c) = served_with_collector(5);
    with_timeout(10, async {
        let res = handle
            .add_and_wait(
                TaskSpec::restartable(make_coop("slow_reg")),
                Duration::from_millis(1),
            )
            .await;
        match res {
            Ok(_) => {}
            Err(RuntimeError::TaskAddTimeout { name, timeout }) => {
                assert_eq!(&*name, "slow_reg");
                assert_eq!(timeout, Duration::from_millis(1));
            }
            other => panic!("unexpected result: {other:?}"),
        }
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn remove_by_id_removes_only_that_id() {
    let (handle, collector) = served_with_collector(5);
    with_timeout(10, async {
        let id_a = handle
            .add_and_wait(
                TaskSpec::restartable(make_coop("a")),
                Duration::from_secs(2),
            )
            .await
            .expect("add a");
        let id_b = handle
            .add_and_wait(
                TaskSpec::restartable(make_coop("b")),
                Duration::from_secs(2),
            )
            .await
            .expect("add b");

        handle.remove(id_a).expect("remove a");

        assert!(
            poll_until(Duration::from_secs(2), || async {
                let list = handle.list().await;
                list.len() == 1 && list[0].0 == id_b && !handle.is_alive("a").await
            })
            .await
        );
        assert!(handle.is_alive("b").await);
        assert!(
            collector
                .by_id(id_a)
                .iter()
                .any(|e| e.kind == EventKind::TaskRemoved)
        );
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn remove_unknown_id_is_noop_emits_task_not_found() {
    let (handle, collector) = served_with_collector(5);
    with_timeout(10, async {
        let id_keep = handle
            .add_and_wait(
                TaskSpec::restartable(make_coop("keep")),
                Duration::from_secs(2),
            )
            .await
            .expect("add keep");
        let stale = stale_id(&handle).await;

        handle.remove(stale).expect("remove stale ok");
        assert!(
            poll_until(Duration::from_secs(2), || async {
                collector.any_reason_contains(EventKind::TaskRemoved, "task_not_found")
            })
            .await
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
        let _ = handle
            .add_and_wait(
                TaskSpec::restartable(make_coop("svc")),
                Duration::from_secs(2),
            )
            .await
            .expect("add svc");

        assert!(handle.remove_by_label("svc").await.expect("remove1"));
        assert!(
            poll_until(Duration::from_secs(2), || async {
                !handle.list().await.iter().any(|(_, l)| &**l == "svc")
            })
            .await
        );
        assert!(!handle.remove_by_label("svc").await.expect("remove2"));
        assert!(
            !handle
                .remove_by_label("never-existed")
                .await
                .expect("remove3")
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
            .add_and_wait(
                TaskSpec::restartable(make_coop("c")),
                Duration::from_secs(2),
            )
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
            .add_and_wait(
                TaskSpec::restartable(make_coop("lbl")),
                Duration::from_secs(2),
            )
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
            .add_and_wait(
                TaskSpec::restartable(make_coop("coop")),
                Duration::from_secs(2),
            )
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
async fn cancel_with_timeout_errors_on_stuck_task() {
    let (handle, _c) = served_with_collector(5);
    with_timeout(10, async {
        let task = TaskFn::arc("stubborn", |_ctx: CancellationToken| async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        });
        let id = handle
            .add_and_wait(TaskSpec::restartable(task), Duration::from_secs(2))
            .await
            .expect("add stubborn");

        match handle
            .cancel_with_timeout(id, Duration::from_millis(150))
            .await
        {
            Err(RuntimeError::TaskRemoveTimeout {
                id: timed_id,
                timeout,
            }) => {
                assert_eq!(timed_id, id);
                assert_eq!(timeout, Duration::from_millis(150));
            }
            other => panic!(
                "expected TaskRemoveTimeout for a task that ignores cancellation, got {other:?}"
            ),
        }
        let _ = with_timeout(5, handle.shutdown()).await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn individually_removed_stuck_task_is_force_aborted_after_grace() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::builder(SupervisorConfig {
        grace: Duration::from_millis(300),
        ..Default::default()
    })
    .with_subscribers(subs)
    .build();
    let handle = sup.serve();

    with_timeout(10, async {
        let task = TaskFn::arc("stuck-runner", |_ctx: CancellationToken| async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        });
        let id = handle
            .add_and_wait(TaskSpec::restartable(task), Duration::from_secs(2))
            .await
            .expect("add stuck-runner");

        handle.remove(id).expect("remove");

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
            .add_and_wait(
                TaskSpec::restartable(make_coop("x")),
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        let id_y = handle
            .add_and_wait(
                TaskSpec::restartable(make_coop("y")),
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        let id_z = handle
            .add_and_wait(
                TaskSpec::restartable(make_coop("z")),
                Duration::from_secs(2),
            )
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

        handle.remove(id_y).expect("remove y");
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
            .add_and_wait(
                TaskSpec::restartable(make_coop("live")),
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        let _ = handle
            .add_and_wait(
                TaskSpec::once(make_ok_once("oneshot")),
                Duration::from_secs(2),
            )
            .await
            .unwrap();

        assert!(
            poll_until(Duration::from_secs(2), || async {
                handle.is_alive("live").await
            })
            .await
        );
        assert!(handle.snapshot().await.iter().any(|n| &**n == "live"));

        assert!(
            poll_until(Duration::from_secs(2), || async {
                !handle.is_alive("oneshot").await
                    && !handle.snapshot().await.iter().any(|n| &**n == "oneshot")
                    && !handle.list().await.iter().any(|(_, l)| &**l == "oneshot")
            })
            .await
        );

        let snap = handle.snapshot().await;
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
        assert!(handle.snapshot().await.is_empty());
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
            .add_and_wait(
                TaskSpec::restartable(make_coop("life")),
                Duration::from_secs(2),
            )
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
            .add_and_wait(
                TaskSpec::restartable(make_coop("reuse")),
                Duration::from_secs(2),
            )
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
            .add_and_wait(
                TaskSpec::restartable(make_coop("reuse")),
                Duration::from_secs(2),
            )
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
        assert!(
            poll_until(Duration::from_secs(2), || async {
                matches!(
                    h2.add(TaskSpec::once(make_ok_once("late"))),
                    Err(RuntimeError::ShuttingDown)
                )
            })
            .await
        );
    })
    .await;
}
