//! Lifecycle & restart-policy integration tests (black-box, public API only).

mod common;

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use common::*;
use taskvisor::BackoffSource;
use taskvisor::prelude::*;

async fn drained(collector: &EventCollector, at_least_removed: usize) {
    assert!(
        collector
            .wait_until(Duration::from_secs(3), |events| {
                events
                    .iter()
                    .filter(|event| event.kind == EventKind::TaskRemoved)
                    .count()
                    >= at_least_removed
            })
            .await,
        "collector never observed {at_least_removed} TaskRemoved event(s)"
    );
}

async fn run_static(specs: Vec<TaskSpec>) -> Arc<EventCollector> {
    let expected_removed = specs.len();
    let (supervisor, collector) = supervisor_with_collector(SupervisorConfig::default());
    with_timeout(10, supervisor.run(specs))
        .await
        .expect("run() should return Ok");
    drained(&collector, expected_removed).await;
    collector
}

#[test]
fn supervisor_builder_is_nameable_from_public_api() {
    let builder: taskvisor::SupervisorBuilder = Supervisor::builder(SupervisorConfig::default());
    let _ = builder;
}

#[tokio::test(flavor = "current_thread")]
async fn never_oneshot_success_emits_attempt_and_typed_task_finish_once() {
    let collector = run_static(vec![TaskSpec::once(make_ok_once("oneshot"))]).await;

    assert_eq!(collector.count(EventKind::AttemptStarting), 1);
    assert_eq!(collector.count(EventKind::AttemptSucceeded), 1);
    assert_eq!(collector.count(EventKind::TaskFinished), 1);
    assert_eq!(collector.count(EventKind::TaskRemoved), 1);
    assert_eq!(collector.count(EventKind::AttemptFailed), 0);
    assert_eq!(collector.count(EventKind::BackoffScheduled), 0);
    let finished = collector.find(EventKind::TaskFinished).unwrap();
    assert_eq!(finished.outcome_kind, Some(TaskOutcomeKind::Completed));
    assert_eq!(finished.reason, None);
    let stopped = collector.find(EventKind::AttemptSucceeded).unwrap();
    assert!(
        stopped.duration_ms.is_some(),
        "terminal AttemptSucceeded must carry attempt duration"
    );
    assert!(
        finished.seq > stopped.seq,
        "TaskFinished must follow the attempt"
    );
    let removed = collector.find(EventKind::TaskRemoved).unwrap();
    assert!(
        removed.seq > finished.seq,
        "TaskRemoved must follow TaskFinished"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn never_oneshot_failure_emits_failed_attempt_then_failed_task() {
    let task = TaskFn::arc("fail-once", |_ctx: TaskContext| async move {
        Err(TaskError::fail("boom".to_string()))
    });
    let collector = run_static(vec![TaskSpec::once(task)]).await;

    assert_eq!(collector.count(EventKind::AttemptStarting), 1);
    assert_eq!(collector.count(EventKind::AttemptFailed), 1);
    assert_eq!(collector.count(EventKind::TaskFinished), 1);
    assert_eq!(collector.count(EventKind::BackoffScheduled), 0);
    assert_eq!(collector.count(EventKind::AttemptSucceeded), 0);

    let failed = collector.find(EventKind::AttemptFailed).unwrap();
    assert!(failed.reason.as_deref().unwrap().contains("boom"));
    let finished = collector.find(EventKind::TaskFinished).unwrap();
    assert_eq!(finished.outcome_kind, Some(TaskOutcomeKind::Failed));
    assert!(finished.reason.as_deref().unwrap().contains("boom"));
}

#[tokio::test(flavor = "current_thread")]
async fn on_failure_flaky_retries_then_succeeds_failure_source_backoff() {
    let remaining = Arc::new(AtomicU32::new(2));
    let r = remaining.clone();
    let task = TaskFn::arc("flaky", move |_ctx: TaskContext| {
        let r = r.clone();
        async move {
            let prev = r.fetch_sub(1, Ordering::SeqCst);
            if prev > 0 {
                Err(TaskError::fail("transient-err".to_string()))
            } else {
                Ok(())
            }
        }
    });
    let spec = TaskSpec::restartable(task).with_backoff(fast_backoff());
    let collector = run_static(vec![spec]).await;

    assert_eq!(collector.count(EventKind::AttemptStarting), 3);
    assert_eq!(collector.count(EventKind::AttemptFailed), 2);
    assert_eq!(collector.count(EventKind::BackoffScheduled), 2);
    assert_eq!(collector.count(EventKind::AttemptSucceeded), 1);
    assert_eq!(collector.count(EventKind::TaskFinished), 1);

    for b in collector.find_all(EventKind::BackoffScheduled) {
        assert_eq!(b.backoff_source, Some(BackoffSource::Failure));
        assert_eq!(b.delay_ms, Some(1));
        assert!(b.reason.as_deref().unwrap().contains("transient-err"));
    }
    let finished = collector.find(EventKind::TaskFinished).unwrap();
    assert_eq!(finished.outcome_kind, Some(TaskOutcomeKind::Completed));
    assert_eq!(finished.reason, None);
}

#[tokio::test(flavor = "current_thread")]
async fn on_failure_fatal_emits_fatal_task_finish_with_exit_code_no_retry() {
    let spec = TaskSpec::restartable(make_fatal("fatal-task", Some(7)));
    let collector = run_static(vec![spec]).await;

    assert_eq!(collector.count(EventKind::AttemptStarting), 1);
    assert_eq!(collector.count(EventKind::AttemptFailed), 1);
    assert_eq!(collector.count(EventKind::TaskFinished), 1);
    assert_eq!(collector.count(EventKind::BackoffScheduled), 0);
    assert_eq!(collector.count(EventKind::AttemptSucceeded), 0);

    assert_eq!(
        collector.find(EventKind::AttemptFailed).unwrap().exit_code,
        Some(7)
    );
    let finished = collector.find(EventKind::TaskFinished).unwrap();
    assert_eq!(finished.outcome_kind, Some(TaskOutcomeKind::Fatal));
    assert_eq!(finished.exit_code, Some(7));
    assert!(
        finished
            .reason
            .as_deref()
            .unwrap()
            .contains("unrecoverable")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn fatal_no_restart_under_always_interval_none() {
    let spec = TaskSpec::restartable(make_fatal("always-fatal", None))
        .with_restart(RestartPolicy::Always { interval: None });
    let collector = run_static(vec![spec]).await;

    assert_eq!(collector.count(EventKind::AttemptStarting), 1);
    assert_eq!(collector.count(EventKind::TaskFinished), 1);
    assert_eq!(
        collector
            .find(EventKind::TaskFinished)
            .unwrap()
            .outcome_kind,
        Some(TaskOutcomeKind::Fatal)
    );
    assert_eq!(collector.count(EventKind::BackoffScheduled), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn max_retries_allows_initial_attempt_plus_configured_retries() {
    for retries in [1, 3] {
        let name = format!("fail-{retries}");
        let spec = TaskSpec::restartable(make_fail(&name, Some(42)))
            .with_backoff(fast_backoff())
            .with_max_retries(NonZeroU32::new(retries).unwrap());
        let collector = run_static(vec![spec]).await;
        let expected_attempts = usize::try_from(retries + 1).unwrap();

        assert_eq!(
            collector.count(EventKind::AttemptStarting),
            expected_attempts,
            "retry limit {retries}"
        );
        assert_eq!(
            collector.count(EventKind::AttemptFailed),
            expected_attempts,
            "retry limit {retries}"
        );
        assert_eq!(
            collector.count(EventKind::BackoffScheduled),
            usize::try_from(retries).unwrap(),
            "retry limit {retries}"
        );
        assert_eq!(collector.count(EventKind::TaskFinished), 1);
        assert!(
            collector
                .find_all(EventKind::AttemptFailed)
                .iter()
                .all(|event| event.exit_code == Some(42)),
            "retry limit {retries}: every failed attempt keeps the exit code"
        );

        let finished = collector.find(EventKind::TaskFinished).unwrap();
        assert_eq!(finished.outcome_kind, Some(TaskOutcomeKind::Failed));
        assert_eq!(finished.exit_code, Some(42));
        assert!(
            finished.reason.as_deref().unwrap().contains("boom"),
            "diagnostic detail should retain the final task error"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn always_interval_none_restarts_repeatedly_no_backoff_scheduled() {
    let (handle, collector) =
        served_with_collector(SupervisorConfig::default().with_grace(Duration::from_secs(2)));

    let counter = Arc::new(AtomicU32::new(0));
    let cc = counter.clone();
    let task = TaskFn::arc("rerun", move |_ctx: TaskContext| {
        let cc = cc.clone();
        async move {
            cc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    });
    let spec = TaskSpec::restartable(task).with_restart(RestartPolicy::Always { interval: None });

    with_timeout(15, async {
        handle.add(spec).await.expect("add ok");
        assert!(
            collector
                .wait_until(Duration::from_secs(5), |events| {
                    counter.load(Ordering::SeqCst) >= 5
                        && events
                            .iter()
                            .filter(|event| event.kind == EventKind::AttemptStarting)
                            .count()
                            >= 5
                })
                .await,
            "immediate-restart loop and its observable start events should reach 5 runs"
        );
        assert_eq!(collector.count(EventKind::BackoffScheduled), 0);
        assert!(collector.count(EventKind::AttemptStarting) >= 5);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn always_interval_some_emits_success_source_backoff_between_runs() {
    let (handle, collector) =
        served_with_collector(SupervisorConfig::default().with_grace(Duration::from_secs(2)));

    let counter = Arc::new(AtomicU32::new(0));
    let cc = counter.clone();
    let task = TaskFn::arc("periodic", move |_ctx: TaskContext| {
        let cc = cc.clone();
        async move {
            cc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    });
    let spec = TaskSpec::restartable(task).with_restart(RestartPolicy::Always {
        interval: Some(Duration::from_millis(5)),
    });

    with_timeout(15, async {
        handle.add(spec).await.expect("add ok");
        assert!(
            collector
                .wait_until(Duration::from_secs(5), |events| {
                    counter.load(Ordering::SeqCst) >= 3
                        && events
                            .iter()
                            .filter(|event| event.kind == EventKind::BackoffScheduled)
                            .count()
                            >= 2
                })
                .await,
            "periodic task and its observable backoff events should reach 3 runs"
        );
        let backoffs = collector.find_all(EventKind::BackoffScheduled);
        assert!(backoffs.len() >= 2, "expected >=2 success-driven backoffs");
        for b in backoffs {
            assert_eq!(b.backoff_source, Some(BackoffSource::Success));
            assert_eq!(b.delay_ms, Some(5));
            assert_eq!(b.reason, None);
        }
        assert_eq!(collector.count(EventKind::AttemptFailed), 0);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn success_driven_restart_does_not_consume_failure_retry_budget() {
    let (handle, collector) =
        served_with_collector(SupervisorConfig::default().with_grace(Duration::from_secs(2)));

    let n = Arc::new(AtomicU32::new(0));
    let nc = n.clone();
    let task = TaskFn::arc("recover", move |_ctx: TaskContext| {
        let nc = nc.clone();
        async move {
            let c = nc.fetch_add(1, Ordering::SeqCst);
            if c == 0 {
                Err(TaskError::fail("first-fails".to_string()))
            } else {
                Ok(())
            }
        }
    });
    let spec = TaskSpec::restartable(task)
        .with_restart(RestartPolicy::Always { interval: None })
        .with_max_retries(NonZeroU32::new(1).unwrap())
        .with_backoff(fast_backoff());

    with_timeout(15, async {
        handle.add(spec).await.expect("add ok");
        assert!(
            collector
                .wait_until(Duration::from_secs(5), |events| {
                    n.load(Ordering::SeqCst) >= 6
                        && events.iter().any(|event| {
                            event.kind == EventKind::BackoffScheduled
                                && event.backoff_source == Some(BackoffSource::Failure)
                        })
                })
                .await,
            "task and its first failure events should settle before assertions"
        );
        assert_eq!(collector.count(EventKind::AttemptFailed), 1);
        let failure_backoffs = collector
            .find_all(EventKind::BackoffScheduled)
            .into_iter()
            .filter(|b| b.backoff_source == Some(BackoffSource::Failure))
            .count();
        assert_eq!(failure_backoffs, 1);
        assert_eq!(collector.count(EventKind::TaskFinished), 0);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn static_run_multiple_oneshots_all_complete_run_returns_ok() {
    let specs = vec![
        TaskSpec::once(make_ok_once("a")),
        TaskSpec::once(make_ok_once("b")),
        TaskSpec::once(make_ok_once("c")),
    ];
    let collector = run_static(specs).await;

    assert_eq!(collector.count(EventKind::AttemptStarting), 3);
    assert_eq!(collector.count(EventKind::AttemptSucceeded), 3);
    assert_eq!(collector.count(EventKind::TaskFinished), 3);
    assert_eq!(collector.count(EventKind::TaskRemoved), 3);

    for label in ["a", "b", "c"] {
        let evs = collector.by_label(label);
        assert!(
            evs.iter().any(|e| e.kind == EventKind::AttemptStarting),
            "missing AttemptStarting for {label}"
        );
        assert!(
            evs.iter().any(|e| {
                e.kind == EventKind::TaskFinished
                    && e.outcome_kind == Some(TaskOutcomeKind::Completed)
            }),
            "missing completed TaskFinished for {label}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_static_batch_starts_no_task_body() {
    let (supervisor, collector) = supervisor_with_collector(SupervisorConfig::default());
    let runs = Arc::new(AtomicU32::new(0));
    let specs = ["unique", "duplicate", "duplicate"]
        .into_iter()
        .map(|label| {
            let runs = Arc::clone(&runs);
            let task = TaskFn::arc(label, move |_ctx: TaskContext| {
                runs.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            });
            TaskSpec::once(task)
        })
        .collect();

    let result = with_timeout(5, supervisor.run(specs)).await;
    assert!(
        matches!(
            result,
            Err(RuntimeError::TaskAlreadyExists { ref name, .. })
                if name.as_ref() == "duplicate"
        ),
        "the duplicate label must reject the full batch: {result:?}"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert!(matches!(
        supervisor.run(vec![]).await,
        Err(RuntimeError::AlreadyRunning)
    ));

    let handle = supervisor.serve();
    handle
        .add(TaskSpec::restartable(make_coop("after-batch-error")))
        .await
        .expect("a rejected batch must leave the runtime open");
    handle
        .shutdown()
        .await
        .expect("the reused runtime must shut down cleanly");

    assert_eq!(collector.count(EventKind::TaskAddRequested), 4);
    assert_eq!(collector.count(EventKind::TaskAddFailed), 3);
    assert_eq!(collector.count(EventKind::TaskAdded), 1);
    assert_eq!(collector.count(EventKind::ShutdownRequested), 1);
    assert_eq!(collector.count(EventKind::AllStoppedWithinGrace), 1);

    for label in ["unique", "duplicate"] {
        assert!(collector.by_label(label).iter().all(|event| {
            !matches!(
                event.kind,
                EventKind::TaskAdded | EventKind::AttemptStarting
            )
        }));
    }

    let unique_failure = collector
        .by_label("unique")
        .into_iter()
        .find(|event| event.kind == EventKind::TaskAddFailed)
        .expect("unique item must receive its batch rejection event");
    assert_eq!(
        unique_failure.rejection_kind,
        Some(RejectionKind::BatchRejected)
    );
    assert_eq!(unique_failure.outcome_kind, Some(TaskOutcomeKind::Rejected));
    let duplicate_kinds: Vec<_> = collector
        .by_label("duplicate")
        .into_iter()
        .filter(|event| event.kind == EventKind::TaskAddFailed)
        .filter_map(|event| event.rejection_kind)
        .collect();
    assert!(duplicate_kinds.contains(&RejectionKind::AlreadyExists));
}
