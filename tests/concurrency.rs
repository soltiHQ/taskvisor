//! Multi-threaded concurrency stress tests.

mod common;

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use taskvisor::prelude::*;

fn served(grace_secs: u64, max_concurrent: usize) -> SupervisorHandle {
    Supervisor::builder(
        SupervisorConfig::default()
            .with_grace(Duration::from_secs(grace_secs))
            .with_max_concurrent(NonZeroUsize::new(max_concurrent)),
    )
    .build()
    .serve()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_storm_unique_names_all_register_then_drain_to_empty() {
    let handle = served(60, 0);
    const N: usize = 256;
    with_timeout(30, async {
        let mut joins = Vec::with_capacity(N);
        for i in 0..N {
            let h = handle.clone();
            joins.push(tokio::spawn(async move {
                h.add(TaskSpec::restartable(make_coop(&format!("w-{i}"))))
                    .await
                    .expect("add")
            }));
        }
        let mut ids = HashSet::new();
        for j in joins {
            ids.insert(j.await.unwrap());
        }
        assert_eq!(ids.len(), N, "all ids must be distinct");

        assert!(
            poll_until(Duration::from_secs(10), || async {
                handle.list().await.len() == N
            })
            .await,
            "all unique-named tasks must register"
        );

        let mut rjoins = Vec::new();
        for id in ids {
            let h = handle.clone();
            rjoins.push(tokio::spawn(async move { h.remove(id).await }));
        }
        for j in rjoins {
            let _ = j.await;
        }
        assert!(
            poll_until(Duration::from_secs(10), || async {
                handle.list().await.is_empty()
            })
            .await,
            "registry must drain to empty"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_storm_duplicate_name_exactly_one_registers() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let handle = Supervisor::builder(SupervisorConfig::default())
        .with_subscribers(subs)
        .build()
        .serve();
    const N: usize = 64;
    with_timeout(30, async {
        let mut joins = Vec::new();
        for _ in 0..N {
            let h = handle.clone();
            joins.push(tokio::spawn(async move {
                h.add(TaskSpec::restartable(make_coop("dup"))).await
            }));
        }
        let mut accepted = 0;
        let mut rejected = 0;
        for j in joins {
            match j.await.unwrap() {
                Ok(_) => accepted += 1,
                Err(RuntimeError::TaskAlreadyExists { .. }) => rejected += 1,
                Err(other) => panic!("unexpected add error: {other:?}"),
            }
        }
        assert_eq!(accepted, 1);
        assert_eq!(rejected, N - 1);

        assert!(
            poll_until(Duration::from_secs(10), || async {
                collector.count(EventKind::TaskAdded) + collector.count(EventKind::TaskAddFailed)
                    == N
            })
            .await,
            "all {N} adds must be processed"
        );
        assert_eq!(collector.count(EventKind::TaskAdded), 1);
        assert_eq!(collector.count(EventKind::TaskAddFailed), N - 1);

        let dup = handle
            .list()
            .await
            .into_iter()
            .filter(|(_, l)| &**l == "dup")
            .count();
        assert_eq!(dup, 1, "exactly one same-named task may register");
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn distinct_ids_minted_concurrently_are_unique() {
    let handle = served(5, 0);
    const N: usize = 50;
    with_timeout(30, async {
        let mut joins = Vec::new();
        for i in 0..N {
            let h = handle.clone();
            joins.push(tokio::spawn(async move {
                h.add(TaskSpec::restartable(make_coop(&format!("task-{i}"))))
                    .await
                    .expect("add")
            }));
        }
        let mut ids = HashSet::new();
        for j in joins {
            ids.insert(j.await.unwrap());
        }
        assert_eq!(ids.len(), N, "concurrently minted ids must be unique");
        assert!(
            poll_until(Duration::from_secs(5), || async {
                handle.list().await.len() == N
            })
            .await
        );
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_name_concurrent_adds_only_one_survives() {
    let handle = served(5, 0);
    const K: usize = 10;
    with_timeout(30, async {
        let mut joins = Vec::new();
        for _ in 0..K {
            let h = handle.clone();
            joins.push(tokio::spawn(async move {
                h.add(TaskSpec::restartable(make_coop("contended"))).await
            }));
        }
        let mut ok = 0;
        let mut already_exists = 0;
        for j in joins {
            match j.await.unwrap() {
                Ok(_) => ok += 1,
                Err(RuntimeError::TaskAlreadyExists { .. }) => already_exists += 1,
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
        assert_eq!(ok, 1, "exactly one concurrent add must win");
        assert_eq!(already_exists, K - 1);
        let count = handle
            .list()
            .await
            .into_iter()
            .filter(|(_, l)| &**l == "contended")
            .count();
        assert_eq!(count, 1);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interleaved_add_and_remove_drains_to_empty() {
    let handle = served(5, 0);
    const N: usize = 200;
    with_timeout(40, async {
        let mut joins = Vec::new();
        for i in 0..N {
            let h = handle.clone();
            joins.push(tokio::spawn(async move {
                let id = h
                    .add(TaskSpec::restartable(make_coop(&format!("t-{i}"))))
                    .await
                    .expect("add");
                let _ = h.remove(id).await;
                id
            }));
        }
        let mut ids = Vec::new();
        for j in joins {
            ids.push(j.await.unwrap());
        }
        for id in ids {
            let _ = handle.remove(id).await;
        }
        assert!(
            poll_until(Duration::from_secs(15), || async {
                handle.list().await.is_empty()
            })
            .await,
            "interleaved add/remove must converge to empty"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_remove_same_id_has_exactly_one_claim() {
    let handle = served(5, 0);
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let task_started = Arc::clone(&started);
    let task_release = Arc::clone(&release);
    let task: TaskRef = TaskFn::arc("remove-race", move |_ctx: TaskContext| {
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
        .expect("register remove-race");
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("remove-race must start");

    const N: usize = 32;
    let mut joins = Vec::with_capacity(N);
    for _ in 0..N {
        let handle = handle.clone();
        joins.push(tokio::spawn(async move {
            handle
                .remove(id)
                .await
                .expect("Remove must receive a reply")
        }));
    }

    let mut claimed = 0;
    for join in joins {
        if join.await.expect("Remove caller must not panic") {
            claimed += 1;
        }
    }
    assert_eq!(claimed, 1, "exactly one Remove may claim the task");
    assert_eq!(handle.list().await, vec![(id, Arc::from("remove-race"))]);

    release.notify_one();
    assert!(
        poll_until(Duration::from_secs(2), || async {
            handle.list().await.is_empty()
        })
        .await,
        "terminal cleanup must remove the retained entry"
    );
    let _ = handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_storm_by_id_returns_true_and_drains() {
    let handle = served(5, 0);
    const N: usize = 128;
    with_timeout(30, async {
        let mut ids = Vec::new();
        for i in 0..N {
            ids.push(
                handle
                    .add(TaskSpec::restartable(make_coop(&format!("c-{i}"))))
                    .await
                    .expect("register"),
            );
        }

        let mut joins = Vec::new();
        for id in ids {
            let h = handle.clone();
            joins.push(tokio::spawn(
                async move { with_timeout(5, h.cancel(id)).await },
            ));
        }
        for j in joins {
            assert!(
                j.await.unwrap().expect("cancel ok"),
                "each cancel must report true"
            );
        }
        assert!(
            poll_until(Duration::from_secs(10), || async {
                handle.list().await.is_empty()
            })
            .await
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_cancel_same_id_returns_exactly_one_true() {
    let handle = served(5, 0);
    const K: usize = 16;
    with_timeout(20, async {
        let id = handle
            .add(TaskSpec::restartable(make_coop("one")))
            .await
            .expect("register");

        let mut joins = Vec::new();
        for _ in 0..K {
            let h = handle.clone();
            joins.push(tokio::spawn(
                async move { with_timeout(5, h.cancel(id)).await },
            ));
        }
        let mut trues = 0;
        for j in joins {
            if j.await.unwrap().expect("cancel ok") {
                trues += 1;
            }
        }
        assert_eq!(
            trues, 1,
            "exactly one concurrent cancel must claim the task"
        );
        assert!(
            poll_until(Duration::from_secs(5), || async {
                handle.list().await.is_empty()
            })
            .await
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rapid_short_lived_once_tasks_alive_tracker_converges_empty() {
    let handle = served(5, 0);
    const M: usize = 300;
    with_timeout(40, async {
        let mut joins = Vec::new();
        for i in 0..M {
            let h = handle.clone();
            joins.push(tokio::spawn(async move {
                h.add(TaskSpec::once(make_ok_once(&format!("o-{i}"))))
                    .await
                    .expect("add")
            }));
        }
        for j in joins {
            let _ = j.await.unwrap();
        }
        assert!(
            poll_until(Duration::from_secs(15), || async {
                handle.list().await.is_empty() && handle.snapshot().await.is_empty()
            })
            .await,
            "registry and alive-tracker must converge to empty"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_storm_with_concurrency_limit_bound_respected_no_deadlock() {
    let handle = served(5, 4);
    const N: usize = 100;
    with_timeout(30, async {
        for i in 0..N {
            handle
                .add(TaskSpec::restartable(make_coop(&format!("lim-{i}"))))
                .await
                .expect("add");
        }
        assert!(
            poll_until(Duration::from_secs(10), || async {
                handle.list().await.len() == N
            })
            .await,
            "all tasks register regardless of the run semaphore"
        );

        for _ in 0..150 {
            let alive = handle.snapshot().await.len();
            assert!(alive <= 4, "alive {alive} exceeded max_concurrent=4");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        with_timeout(8, handle.shutdown())
            .await
            .expect("shutdown ok");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_then_immediate_shutdown_storm_returns_within_grace() {
    let handle = served(5, 0);
    const N: usize = 150;
    with_timeout(20, async {
        let mut adds = Vec::with_capacity(N);
        for i in 0..N {
            let h = handle.clone();
            adds.push(tokio::spawn(async move {
                h.add(TaskSpec::restartable(make_coop(&format!("s-{i}"))))
                    .await
            }));
        }
        let (shutdown, add_results) = tokio::join!(with_timeout(10, handle.shutdown()), async {
            let mut results = Vec::with_capacity(N);
            for add in adds {
                results.push(add.await.expect("add task must not panic"));
            }
            results
        });
        for result in add_results {
            assert!(
                result.is_ok() || matches!(result, Err(RuntimeError::ShuttingDown)),
                "concurrent add must be accepted or rejected by shutdown: {result:?}"
            );
        }
        match shutdown {
            Ok(()) => {}
            Err(RuntimeError::GraceExceeded { .. }) => {}
            other => panic!("shutdown must return Ok or GraceExceeded, got {other:?}"),
        }
    })
    .await;
}

#[cfg(feature = "controller")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn controller_many_distinct_slots_all_settle() {
    use taskvisor::{ControllerConfig, ControllerSpec};

    let handle =
        Supervisor::builder(SupervisorConfig::default().with_grace(Duration::from_secs(5)))
            .with_controller(ControllerConfig::default())
            .build()
            .serve();
    const S: usize = 128;
    with_timeout(40, async {
        let mut joins = Vec::new();
        for s in 0..S {
            let h = handle.clone();
            joins.push(tokio::spawn(async move {
                let spec = TaskSpec::restartable(make_coop(&format!("svc-{s}")));
                h.submit(ControllerSpec::queue(spec).with_slot(format!("slot-{s}")))
                    .await
            }));
        }
        for j in joins {
            j.await.unwrap().expect("submit ok");
        }
        assert!(
            poll_until(Duration::from_secs(15), || async {
                let snap = handle.snapshot().await;
                (0..S).all(|s| snap.iter().any(|n| **n == format!("svc-{s}")))
            })
            .await,
            "all distinct-slot tasks must become alive"
        );
        with_timeout(8, handle.shutdown())
            .await
            .expect("shutdown ok");
    })
    .await;
}

#[cfg(feature = "controller")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn controller_replace_storm_single_slot_one_alive() {
    use taskvisor::{ControllerConfig, ControllerSpec};

    let handle =
        Supervisor::builder(SupervisorConfig::default().with_grace(Duration::from_secs(5)))
            .with_controller(ControllerConfig::default())
            .build()
            .serve();
    const K: usize = 50;
    with_timeout(40, async {
        let mut joins = Vec::new();
        for i in 0..K {
            let h = handle.clone();
            joins.push(tokio::spawn(async move {
                let spec = TaskSpec::restartable(make_coop(&format!("run-{i}")));
                h.submit(ControllerSpec::replace(spec).with_slot("s")).await
            }));
        }
        for j in joins {
            j.await.unwrap().expect("submit ok");
        }
        let alive_in_family = || async {
            handle
                .snapshot()
                .await
                .iter()
                .filter(|n| n.starts_with("run-"))
                .count()
        };
        assert!(
            poll_until(Duration::from_secs(15), || async {
                alive_in_family().await == 1 && {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    alive_in_family().await == 1
                }
            })
            .await,
            "exactly one replacement may run in the shared slot"
        );
        with_timeout(8, handle.shutdown())
            .await
            .expect("shutdown ok");
    })
    .await;
}

#[cfg(feature = "controller")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn controller_drop_if_running_storm_one_runs_rest_rejected() {
    use taskvisor::{ControllerConfig, ControllerSpec};

    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let handle =
        Supervisor::builder(SupervisorConfig::default().with_grace(Duration::from_secs(5)))
            .with_subscribers(subs)
            .with_controller(ControllerConfig::default())
            .build()
            .serve();
    const K: usize = 40;
    with_timeout(30, async {
        let mut joins = Vec::new();
        for i in 0..K {
            let h = handle.clone();
            joins.push(tokio::spawn(async move {
                let spec = TaskSpec::restartable(make_coop(&format!("d-{i}")));
                h.submit(ControllerSpec::drop_if_running(spec).with_slot("s"))
                    .await
            }));
        }
        for j in joins {
            j.await.unwrap().expect("submit ok");
        }
        assert!(
            poll_until(Duration::from_secs(10), || async {
                let alive = handle
                    .snapshot()
                    .await
                    .iter()
                    .filter(|n| n.starts_with("d-"))
                    .count();
                alive == 1 && collector.count(EventKind::ControllerRejected) >= 1
            })
            .await,
            "exactly one task runs; the rest are rejected"
        );
        for _ in 0..50 {
            let alive = handle
                .snapshot()
                .await
                .iter()
                .filter(|n| n.starts_with("d-"))
                .count();
            assert!(
                alive <= 1,
                "DropIfRunning must never allow two concurrent occupants"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        with_timeout(8, handle.shutdown())
            .await
            .expect("shutdown ok");
    })
    .await;
}
