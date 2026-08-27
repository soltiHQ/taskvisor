//! Runtime-identity & dynamic task-management integration tests.

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::*;
use taskvisor::TaskTarget;
use taskvisor::prelude::*;

struct CountedTarget {
    conversions: Arc<AtomicUsize>,
}

impl From<CountedTarget> for TaskTarget {
    fn from(target: CountedTarget) -> Self {
        target.conversions.fetch_add(1, Ordering::SeqCst);
        TaskTarget::from("missing-counted-target")
    }
}

async fn stale_id(handle: &SupervisorHandle) -> TaskId {
    let id = handle
        .add(TaskSpec::restartable("throwaway", make_coop()))
        .execute()
        .await
        .expect("add ok");
    assert!(handle.cancel(id).execute().await.expect("cancel ok"));
    assert!(
        poll_until(Duration::from_secs(2), || async {
            !handle.list().await.iter().any(|(i, _)| *i == id)
        })
        .await
    );
    id
}

#[tokio::test(flavor = "current_thread")]
async fn management_target_conversion_starts_only_when_execute_is_polled() {
    let (handle, _collector) = served_with_collector_and_grace(5);
    let conversions = Arc::new(AtomicUsize::new(0));

    let remove = handle.remove(CountedTarget {
        conversions: Arc::clone(&conversions),
    });
    assert_eq!(conversions.load(Ordering::SeqCst), 0);
    assert!(format!("{remove:?}").contains("RemoveOperation"));
    assert_eq!(conversions.load(Ordering::SeqCst), 0);
    drop(remove);
    assert_eq!(conversions.load(Ordering::SeqCst), 0);

    let cancel_future = handle
        .cancel(CountedTarget {
            conversions: Arc::clone(&conversions),
        })
        .execute();
    assert_eq!(conversions.load(Ordering::SeqCst), 0);
    drop(cancel_future);
    assert_eq!(conversions.load(Ordering::SeqCst), 0);

    let remove_future = handle
        .remove(CountedTarget {
            conversions: Arc::clone(&conversions),
        })
        .execute();
    assert_eq!(conversions.load(Ordering::SeqCst), 0);
    assert!(!remove_future.await.expect("missing target is a no-op"));
    assert_eq!(conversions.load(Ordering::SeqCst), 1);

    handle.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "current_thread")]
async fn add_confirms_registration_returns_id_and_starts_task() {
    let (handle, collector) = served_with_collector_and_grace(5);
    with_timeout(10, async {
        let id = handle
            .add(TaskSpec::restartable("worker", make_coop()))
            .execute()
            .await
            .expect("add ok");
        assert!(
            poll_until(Duration::from_secs(2), || async {
                handle.is_alive("worker").await
                    && collector.by_id(id).iter().any(|e| {
                        e.kind == EventKind::AttemptStarting && e.task.as_deref() == Some("worker")
                    })
            })
            .await
        );
        let list = handle.list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, id);
        assert_eq!(&*list[0].1, "worker");
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_add_returns_error_and_only_first_runs() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (handle, _collector) = served_with_collector_and_grace(5);
    with_timeout(10, async {
        let id1 = handle
            .add(TaskSpec::restartable("dup", make_coop()))
            .execute()
            .await
            .expect("add1");

        let rejected_runs = Arc::new(AtomicUsize::new(0));
        let task_runs = Arc::clone(&rejected_runs);
        let rejected: TaskRef = TaskFn::arc(move |_ctx: TaskContext| {
            task_runs.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        });
        let error = handle
            .add(TaskSpec::once("dup", rejected))
            .execute()
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
async fn fast_task_registration_has_no_library_timeout() {
    let (handle, _c) = served_with_collector_and_grace(5);
    with_timeout(10, async {
        let id = handle
            .add(TaskSpec::once("fast-registration", make_ok_once()))
            .execute()
            .await
            .expect("registry reply must confirm even a fast task");
        assert!(id.get() > 0);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn remove_by_id_removes_only_that_id() {
    let (handle, collector) = served_with_collector_and_grace(5);
    with_timeout(10, async {
        let id_a = handle
            .add(TaskSpec::restartable("a", make_coop()))
            .execute()
            .await
            .expect("add a");
        let id_b = handle
            .add(TaskSpec::restartable("b", make_coop()))
            .execute()
            .await
            .expect("add b");

        assert!(handle.remove(id_a).execute().await.expect("remove a"));

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
    let (handle, collector) = served_with_collector_and_grace(5);
    with_timeout(10, async {
        let id_keep = handle
            .add(TaskSpec::restartable("keep", make_coop()))
            .execute()
            .await
            .expect("add keep");
        let stale = stale_id(&handle).await;
        assert!(
            collector
                .wait_until(Duration::from_secs(2), |events| {
                    events.iter().any(|event| {
                        event.id == Some(stale) && event.kind == EventKind::TaskRemoved
                    })
                })
                .await,
            "the original cancellation must publish its terminal removal"
        );
        let removed_before = collector
            .by_id(stale)
            .iter()
            .filter(|event| event.kind == EventKind::TaskRemoved)
            .count();

        assert!(
            !handle
                .remove(stale)
                .execute()
                .await
                .expect("remove stale ok")
        );
        assert!(
            !handle
                .remove(stale)
                .fail_fast()
                .execute()
                .await
                .expect("fail-fast remove stale ok")
        );
        let barrier = handle
            .add(TaskSpec::restartable("remove-unknown-barrier", make_coop()))
            .execute()
            .await
            .expect("later add confirms that the unknown remove was processed");
        assert!(
            collector
                .wait_until(Duration::from_secs(2), |events| {
                    events.iter().any(|event| {
                        event.id == Some(barrier) && event.kind == EventKind::TaskAdded
                    })
                })
                .await,
            "the subscriber must observe the post-remove barrier"
        );
        let removed_after = collector
            .by_id(stale)
            .iter()
            .filter(|event| event.kind == EventKind::TaskRemoved)
            .count();
        assert_eq!(
            removed_after, removed_before,
            "unknown removals must not invent another terminal transition"
        );
        let list = handle.list().await;
        assert!(list.iter().any(|(i, l)| *i == id_keep && &**l == "keep"));
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn remove_name_target_returns_true_then_false() {
    let (handle, _c) = served_with_collector_and_grace(5);
    with_timeout(10, async {
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let task_started = Arc::clone(&started);
        let task_release = Arc::clone(&release);
        let task: TaskRef = TaskFn::arc(move |_ctx: TaskContext| {
            let started = Arc::clone(&task_started);
            let release = Arc::clone(&task_release);
            async move {
                started.notify_one();
                release.notified().await;
                Ok(())
            }
        });
        let id = handle
            .add(TaskSpec::restartable("svc", task))
            .execute()
            .await
            .expect("add svc");
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .expect("svc must start before removal");

        assert!(handle.remove("svc").execute().await.expect("remove1"));
        assert!(
            !handle
                .remove("svc")
                .fail_fast()
                .execute()
                .await
                .expect("remove2")
        );
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
                .remove("never-existed")
                .execute()
                .await
                .expect("remove unknown name")
        );
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_by_id_true_then_false_on_double_cancel() {
    let (handle, _c) = served_with_collector_and_grace(5);
    with_timeout(10, async {
        let id = handle
            .add(TaskSpec::restartable("c", make_coop()))
            .execute()
            .await
            .expect("add c");
        assert!(handle.cancel(id).execute().await.expect("cancel1"));
        assert!(!handle.cancel(id).execute().await.expect("cancel2"));
        assert!(!handle.is_alive("c").await);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_name_target_returns_true_then_false() {
    let (handle, _c) = served_with_collector_and_grace(5);
    with_timeout(10, async {
        let _ = handle
            .add(TaskSpec::restartable("lbl", make_coop()))
            .execute()
            .await
            .expect("add lbl");
        assert!(handle.cancel("lbl").execute().await.expect("c1"));
        assert!(
            poll_until(Duration::from_secs(2), || async {
                !handle.list().await.iter().any(|(_, l)| &**l == "lbl")
            })
            .await
        );
        assert!(!handle.cancel("lbl").execute().await.expect("c2"));
        assert!(!handle.cancel("ghost").execute().await.expect("c3"));
        assert!(!handle.is_alive("lbl").await);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn by_name_operations_return_false_for_missing_tasks() {
    let (handle, _c) = served_with_collector_and_grace(5);
    with_timeout(10, async {
        assert!(!handle.remove("missing").execute().await.expect("remove"));
        assert!(
            !handle
                .remove("missing")
                .fail_fast()
                .execute()
                .await
                .expect("try remove")
        );
        assert!(!handle.cancel("missing").execute().await.expect("cancel"));
        assert!(
            !handle
                .cancel("missing")
                .fail_fast()
                .execute()
                .await
                .expect("try cancel")
        );
        assert!(
            !handle
                .cancel("missing")
                .termination_timeout(Duration::ZERO)
                .execute()
                .await
                .expect("timed cancel")
        );
        assert!(
            !handle
                .cancel("missing")
                .termination_timeout(Duration::ZERO)
                .fail_fast()
                .execute()
                .await
                .expect("try timed cancel")
        );
        handle.shutdown().await.expect("shutdown");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn timed_cancel_variants_are_public_contracts() {
    let (handle, _c) = served_with_collector_and_grace(5);
    with_timeout(10, async {
        let id = handle
            .add(TaskSpec::restartable("coop", make_coop()))
            .execute()
            .await
            .expect("add coop");
        assert!(
            handle
                .cancel(id)
                .termination_timeout(Duration::from_secs(2))
                .execute()
                .await
                .expect("cancel with termination timeout")
        );
        assert!(!handle.is_alive("coop").await);

        let _ = handle
            .add(TaskSpec::restartable("label-timeout", make_coop()))
            .execute()
            .await
            .expect("add label-timeout");
        assert!(
            handle
                .cancel("label-timeout")
                .termination_timeout(Duration::from_secs(2))
                .execute()
                .await
                .expect("cancel name target with termination timeout")
        );

        let _ = handle
            .add(TaskSpec::restartable("try-label-timeout", make_coop()))
            .execute()
            .await
            .expect("add try-label-timeout");
        assert!(
            handle
                .cancel("try-label-timeout")
                .termination_timeout(Duration::from_secs(2))
                .fail_fast()
                .execute()
                .await
                .expect("fail-fast cancel name target with termination timeout")
        );

        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_timeout_does_not_stop_shared_removal() {
    let (handle, _c) = served_with_collector_and_grace(5);
    with_timeout(10, async {
        let started = Arc::new(tokio::sync::Notify::new());
        let cancellation_seen = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let started_by_task = Arc::clone(&started);
        let seen_by_task = Arc::clone(&cancellation_seen);
        let release_by_task = Arc::clone(&release);
        let task = TaskFn::arc(move |ctx: TaskContext| {
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
            .add(TaskSpec::restartable("timeout-shared", task)).execute()
            .await
            .expect("add timeout-shared");
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .expect("task must start before cancellation");

        match handle.cancel(id).termination_timeout(Duration::ZERO).execute().await {
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

        let mut joined_cancel = Box::pin(handle.cancel(id).execute());
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
    let (handle, collector) =
        served_with_collector(SupervisorConfig::default().with_grace(Duration::from_millis(300)));

    with_timeout(10, async {
        let (task, started) = make_stubborn();
        let id = handle
            .add(TaskSpec::restartable("stuck-runner", task))
            .execute()
            .await
            .expect("add stuck-runner");
        wait_for_start("stuck-runner", &started).await;

        assert!(handle.remove(id).execute().await.expect("remove"));

        assert!(
            collector
                .wait_until(Duration::from_secs(3), |events| {
                    events
                        .iter()
                        .any(|event| event.id == Some(id) && event.kind == EventKind::TaskRemoved)
                })
                .await,
            "stuck task must be force-aborted after grace, not leaked"
        );
        assert_eq!(
            collector
                .by_id(id)
                .iter()
                .filter(|event| {
                    event.kind == EventKind::TaskFinished
                        && event.outcome_kind == Some(TaskOutcomeKind::ForceAborted)
                })
                .count(),
            1
        );
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn list_reflects_registered_set_sorted_by_id() {
    let (handle, _c) = served_with_collector_and_grace(5);
    with_timeout(10, async {
        let id_x = handle
            .add(TaskSpec::restartable("x", make_coop()))
            .execute()
            .await
            .unwrap();
        let id_y = handle
            .add(TaskSpec::restartable("y", make_coop()))
            .execute()
            .await
            .unwrap();
        let id_z = handle
            .add(TaskSpec::restartable("z", make_coop()))
            .execute()
            .await
            .unwrap();

        let list = handle.list().await;
        assert_eq!(list.len(), 3);
        let ids: Vec<TaskId> = list.iter().map(|(i, _)| *i).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "list must be sorted ascending by id");
        let names: HashSet<&str> = list.iter().map(|(_, l)| &**l).collect();
        assert_eq!(names, HashSet::from(["x", "y", "z"]));
        assert!(ids.contains(&id_x) && ids.contains(&id_y) && ids.contains(&id_z));

        assert!(handle.remove(id_y).execute().await.expect("remove y"));
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
    let (handle, _c) = served_with_collector_and_grace(5);
    with_timeout(10, async {
        assert!(!handle.is_alive("never-registered").await);
        assert!(handle.alive_snapshot().await.is_empty());
        assert!(handle.list().await.is_empty());

        let _ = handle
            .add(TaskSpec::restartable("live", make_coop()))
            .execute()
            .await
            .unwrap();
        let _ = handle
            .add(TaskSpec::once("oneshot", make_ok_once()))
            .execute()
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
async fn events_carry_correct_id_across_full_lifecycle() {
    let (handle, collector) = served_with_collector_and_grace(5);
    with_timeout(10, async {
        let id = handle
            .add(TaskSpec::restartable("life", make_coop()))
            .execute()
            .await
            .unwrap();
        assert!(handle.cancel(id).execute().await.expect("cancel"));

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
        assert!(by_id.iter().any(|e| e.kind == EventKind::AttemptStarting));
        assert!(by_id.iter().any(|e| e.kind == EventKind::TaskRemoved));
        for e in collector.by_name("life") {
            if let Some(eid) = e.id {
                assert_eq!(eid, id, "event {:?} carried a foreign id", e.kind);
            }
        }
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn re_add_same_name_after_removal_succeeds_with_new_id() {
    let (handle, _c) = served_with_collector_and_grace(5);
    with_timeout(10, async {
        let id1 = handle
            .add(TaskSpec::restartable("reuse", make_coop()))
            .execute()
            .await
            .unwrap();
        assert!(handle.cancel(id1).execute().await.expect("cancel"));
        assert!(
            poll_until(Duration::from_secs(2), || async {
                !handle.list().await.iter().any(|(_, l)| &**l == "reuse")
            })
            .await
        );
        let id2 = handle
            .add(TaskSpec::restartable("reuse", make_coop()))
            .execute()
            .await
            .unwrap();
        assert_ne!(id1, id2, "re-added name must get a fresh id");
        let list = handle.list().await;
        assert!(list.iter().any(|(i, l)| *i == id2 && &**l == "reuse"));
        assert!(!list.iter().any(|(i, _)| *i == id1));
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn add_after_shutdown_returns_shutting_down() {
    let (handle, _c) = served_with_collector_and_grace(5);
    let h2 = handle.clone();
    with_timeout(10, async {
        with_timeout(5, handle.shutdown())
            .await
            .expect("shutdown ok");
        assert!(matches!(
            h2.add(TaskSpec::once("late", make_ok_once()))
                .execute()
                .await,
            Err(RuntimeError::ShuttingDown)
        ));
    })
    .await;
}
