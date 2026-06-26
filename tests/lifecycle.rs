//! Lifecycle & restart-policy integration tests (black-box, public API only).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use common::*;
use taskvisor::BackoffSource;
use taskvisor::prelude::*;

async fn drained(collector: &EventCollector, at_least_removed: usize) {
    assert!(
        poll_until(Duration::from_secs(3), || async {
            collector.count(EventKind::TaskRemoved) >= at_least_removed
        })
        .await,
        "collector never observed {at_least_removed} TaskRemoved event(s)"
    );
}

#[test]
fn supervisor_builder_is_nameable_from_public_api() {
    // The builder must be a public, nameable type: users store it in variables,
    // write helper functions returning it, and docs.rs must render its page.
    let builder: taskvisor::SupervisorBuilder = Supervisor::builder(SupervisorConfig::default());
    let _ = builder;
}

#[tokio::test(flavor = "current_thread")]
async fn never_oneshot_success_emits_starting_stopped_exhausted_once() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::new(SupervisorConfig::default(), subs);

    let spec = TaskSpec::once(make_ok_once("oneshot"));
    with_timeout(10, sup.run(vec![spec]))
        .await
        .expect("run() should return Ok");

    drained(&collector, 1).await;

    assert_eq!(collector.count(EventKind::TaskStarting), 1);
    assert_eq!(collector.count(EventKind::TaskStopped), 1);
    assert_eq!(collector.count(EventKind::ActorExhausted), 1);
    assert_eq!(collector.count(EventKind::TaskRemoved), 1);
    assert_eq!(collector.count(EventKind::TaskFailed), 0);
    assert_eq!(collector.count(EventKind::BackoffScheduled), 0);
    assert_eq!(collector.count(EventKind::ActorDead), 0);

    let exhausted = collector.find(EventKind::ActorExhausted).unwrap();
    assert_eq!(
        exhausted.reason.as_deref(),
        Some("policy_exhausted_success")
    );
    let stopped = collector.find(EventKind::TaskStopped).unwrap();
    assert!(exhausted.seq > stopped.seq, "exhausted must follow stopped");
}

#[tokio::test(flavor = "current_thread")]
async fn never_oneshot_failure_emits_taskfailed_then_exhausted_no_backoff() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::new(SupervisorConfig::default(), subs);

    let task = TaskFn::arc("fail-once", |_ctx: TaskContext| async move {
        Err(TaskError::Fail {
            reason: "boom".to_string(),
            exit_code: None,
        })
    });
    with_timeout(10, sup.run(vec![TaskSpec::once(task)]))
        .await
        .expect("run() should return Ok");

    drained(&collector, 1).await;

    assert_eq!(collector.count(EventKind::TaskStarting), 1);
    assert_eq!(collector.count(EventKind::TaskFailed), 1);
    assert_eq!(collector.count(EventKind::ActorExhausted), 1);
    assert_eq!(collector.count(EventKind::BackoffScheduled), 0);
    assert_eq!(collector.count(EventKind::ActorDead), 0);
    assert_eq!(collector.count(EventKind::TaskStopped), 0);

    let failed = collector.find(EventKind::TaskFailed).unwrap();
    assert!(failed.reason.as_deref().unwrap().contains("boom"));
    let exhausted = collector.find(EventKind::ActorExhausted).unwrap();
    assert!(
        !exhausted
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("max_retries_exceeded")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn on_failure_flaky_retries_then_succeeds_failure_source_backoff() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::new(SupervisorConfig::default(), subs);

    let remaining = Arc::new(AtomicU32::new(2));
    let r = remaining.clone();
    let task = TaskFn::arc("flaky", move |_ctx: TaskContext| {
        let r = r.clone();
        async move {
            let prev = r.fetch_sub(1, Ordering::SeqCst);
            if prev > 0 {
                Err(TaskError::Fail {
                    reason: "transient-err".to_string(),
                    exit_code: None,
                })
            } else {
                Ok(())
            }
        }
    });
    let spec = TaskSpec::restartable(task).with_backoff(fast_backoff());
    with_timeout(10, sup.run(vec![spec]))
        .await
        .expect("run() should return Ok");

    drained(&collector, 1).await;

    assert_eq!(collector.count(EventKind::TaskStarting), 3);
    assert_eq!(collector.count(EventKind::TaskFailed), 2);
    assert_eq!(collector.count(EventKind::BackoffScheduled), 2);
    assert_eq!(collector.count(EventKind::TaskStopped), 1);
    assert_eq!(collector.count(EventKind::ActorExhausted), 1);
    assert_eq!(collector.count(EventKind::ActorDead), 0);

    for b in collector.find_all(EventKind::BackoffScheduled) {
        assert_eq!(b.backoff_source, Some(BackoffSource::Failure));
        assert_eq!(b.delay_ms, Some(1));
        assert!(b.reason.as_deref().unwrap().contains("transient-err"));
    }
    let exhausted = collector.find(EventKind::ActorExhausted).unwrap();
    assert_eq!(
        exhausted.reason.as_deref(),
        Some("policy_exhausted_success")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn on_failure_fatal_emits_actordead_with_exit_code_no_retry() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::new(SupervisorConfig::default(), subs);

    let spec = TaskSpec::restartable(make_fatal("fatal-task", Some(7)));
    with_timeout(10, sup.run(vec![spec]))
        .await
        .expect("run() should return Ok");

    drained(&collector, 1).await;

    assert_eq!(collector.count(EventKind::TaskStarting), 1);
    assert_eq!(collector.count(EventKind::TaskFailed), 1);
    assert_eq!(collector.count(EventKind::ActorDead), 1);
    assert_eq!(collector.count(EventKind::BackoffScheduled), 0);
    assert_eq!(collector.count(EventKind::ActorExhausted), 0);
    assert_eq!(collector.count(EventKind::TaskStopped), 0);

    assert_eq!(
        collector.find(EventKind::TaskFailed).unwrap().exit_code,
        Some(7)
    );
    let dead = collector.find(EventKind::ActorDead).unwrap();
    assert_eq!(dead.exit_code, Some(7));
    assert!(dead.reason.as_deref().unwrap().contains("unrecoverable"));
}

#[tokio::test(flavor = "current_thread")]
async fn fatal_no_restart_under_always_interval_none() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::new(SupervisorConfig::default(), subs);

    let spec = TaskSpec::restartable(make_fatal("always-fatal", None))
        .with_restart(RestartPolicy::Always { interval: None });
    with_timeout(5, sup.run(vec![spec]))
        .await
        .expect("run() should return (Fatal must short-circuit Always restart)");

    drained(&collector, 1).await;

    assert_eq!(collector.count(EventKind::TaskStarting), 1);
    assert_eq!(collector.count(EventKind::ActorDead), 1);
    assert_eq!(collector.count(EventKind::BackoffScheduled), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn max_retries_three_yields_four_runs_then_exhausted() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::new(SupervisorConfig::default(), subs);

    let spec = TaskSpec::restartable(make_fail("always-fail", Some(42)))
        .with_backoff(fast_backoff())
        .with_max_retries(3);
    with_timeout(10, sup.run(vec![spec]))
        .await
        .expect("run() should return Ok");

    drained(&collector, 1).await;

    assert_eq!(collector.count(EventKind::TaskStarting), 4);
    assert_eq!(collector.count(EventKind::TaskFailed), 4);
    assert_eq!(collector.count(EventKind::BackoffScheduled), 3);
    assert_eq!(collector.count(EventKind::ActorExhausted), 1);
    assert_eq!(collector.count(EventKind::ActorDead), 0);

    for f in collector.find_all(EventKind::TaskFailed) {
        assert_eq!(f.exit_code, Some(42));
    }
    let exhausted = collector.find(EventKind::ActorExhausted).unwrap();
    assert_eq!(exhausted.exit_code, Some(42));
    let reason = exhausted.reason.as_deref().unwrap();
    assert!(reason.contains("max_retries_exceeded"), "got: {reason}");
    assert!(reason.contains("(3/3)"), "got: {reason}");
}

#[tokio::test(flavor = "current_thread")]
async fn max_retries_one_yields_two_runs_boundary() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::new(SupervisorConfig::default(), subs);

    let spec = TaskSpec::restartable(make_fail("fail-1", None))
        .with_backoff(fast_backoff())
        .with_max_retries(1);
    with_timeout(10, sup.run(vec![spec]))
        .await
        .expect("run() should return Ok");

    drained(&collector, 1).await;

    assert_eq!(collector.count(EventKind::TaskStarting), 2);
    assert_eq!(collector.count(EventKind::TaskFailed), 2);
    assert_eq!(collector.count(EventKind::BackoffScheduled), 1);
    assert_eq!(collector.count(EventKind::ActorExhausted), 1);

    let reason = collector
        .find(EventKind::ActorExhausted)
        .unwrap()
        .reason
        .as_deref()
        .unwrap()
        .to_string();
    assert!(reason.contains("max_retries_exceeded"), "got: {reason}");
    assert!(reason.contains("(1/1)"), "got: {reason}");
}

#[tokio::test(flavor = "current_thread")]
async fn unlimited_retries_eventual_success_no_max_retries_reason() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::new(SupervisorConfig::default(), subs);

    let n = Arc::new(AtomicU32::new(0));
    let nc = n.clone();
    let task = TaskFn::arc("eventual", move |_ctx: TaskContext| {
        let nc = nc.clone();
        async move {
            let c = nc.fetch_add(1, Ordering::SeqCst);
            if c < 3 {
                Err(TaskError::Fail {
                    reason: "still-failing".to_string(),
                    exit_code: None,
                })
            } else {
                Ok(())
            }
        }
    });
    let spec = TaskSpec::restartable(task).with_backoff(fast_backoff());
    with_timeout(10, sup.run(vec![spec]))
        .await
        .expect("run() should return Ok");

    drained(&collector, 1).await;

    assert_eq!(collector.count(EventKind::TaskFailed), 3);
    assert_eq!(collector.count(EventKind::BackoffScheduled), 3);
    assert_eq!(collector.count(EventKind::TaskStopped), 1);
    assert_eq!(collector.count(EventKind::ActorExhausted), 1);
    assert_eq!(collector.count(EventKind::ActorDead), 0);

    for b in collector.find_all(EventKind::BackoffScheduled) {
        assert_eq!(b.backoff_source, Some(BackoffSource::Failure));
    }
    assert!(
        !collector.any_reason_contains(EventKind::ActorExhausted, "max_retries_exceeded"),
        "unlimited retries must never exhaust on max-retries"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn always_interval_none_restarts_repeatedly_no_backoff_scheduled() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::builder(SupervisorConfig {
        grace: Duration::from_secs(2),
        ..Default::default()
    })
    .with_subscribers(subs)
    .build();
    let handle = sup.serve();

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
        handle
            .add_and_wait(spec, Duration::from_secs(1))
            .await
            .expect("add_and_wait ok");
        assert!(
            poll_until(Duration::from_secs(5), || async {
                counter.load(Ordering::SeqCst) >= 5
            })
            .await,
            "immediate-restart loop should re-run at least 5 times"
        );
        assert_eq!(collector.count(EventKind::BackoffScheduled), 0);
        assert!(collector.count(EventKind::TaskStarting) >= 5);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn always_interval_some_emits_success_source_backoff_between_runs() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::builder(SupervisorConfig {
        grace: Duration::from_secs(2),
        ..Default::default()
    })
    .with_subscribers(subs)
    .build();
    let handle = sup.serve();

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
        handle
            .add_and_wait(spec, Duration::from_secs(1))
            .await
            .expect("add_and_wait ok");
        assert!(
            poll_until(Duration::from_secs(5), || async {
                counter.load(Ordering::SeqCst) >= 3
            })
            .await,
            "periodic task should re-run at least 3 times"
        );
        let backoffs = collector.find_all(EventKind::BackoffScheduled);
        assert!(backoffs.len() >= 2, "expected >=2 success-driven backoffs");
        for b in backoffs {
            assert_eq!(b.backoff_source, Some(BackoffSource::Success));
            assert_eq!(b.delay_ms, Some(5));
            assert_eq!(b.reason, None);
        }
        assert_eq!(collector.count(EventKind::TaskFailed), 0);
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn success_driven_restart_does_not_consume_failure_retry_budget() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::builder(SupervisorConfig {
        grace: Duration::from_secs(2),
        ..Default::default()
    })
    .with_subscribers(subs)
    .build();
    let handle = sup.serve();

    let n = Arc::new(AtomicU32::new(0));
    let nc = n.clone();
    let task = TaskFn::arc("recover", move |_ctx: TaskContext| {
        let nc = nc.clone();
        async move {
            let c = nc.fetch_add(1, Ordering::SeqCst);
            if c == 0 {
                Err(TaskError::Fail {
                    reason: "first-fails".to_string(),
                    exit_code: None,
                })
            } else {
                Ok(())
            }
        }
    });
    let spec = TaskSpec::restartable(task)
        .with_restart(RestartPolicy::Always { interval: None })
        .with_max_retries(1)
        .with_backoff(fast_backoff());

    with_timeout(15, async {
        handle
            .add_and_wait(spec, Duration::from_secs(1))
            .await
            .expect("add_and_wait ok");
        assert!(
            poll_until(Duration::from_secs(5), || async {
                n.load(Ordering::SeqCst) >= 6
            })
            .await,
            "task should keep restarting on success despite max_retries=1"
        );
        assert_eq!(collector.count(EventKind::TaskFailed), 1);
        let failure_backoffs = collector
            .find_all(EventKind::BackoffScheduled)
            .into_iter()
            .filter(|b| b.backoff_source == Some(BackoffSource::Failure))
            .count();
        assert_eq!(failure_backoffs, 1);
        assert!(
            !collector.any_reason_contains(EventKind::ActorExhausted, "max_retries_exceeded"),
            "budget reset means it must never exhaust on max-retries"
        );
        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn static_run_multiple_oneshots_all_complete_run_returns_ok() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::new(SupervisorConfig::default(), subs);

    let specs = vec![
        TaskSpec::once(make_ok_once("a")),
        TaskSpec::once(make_ok_once("b")),
        TaskSpec::once(make_ok_once("c")),
    ];
    with_timeout(10, sup.run(specs))
        .await
        .expect("run() should return Ok");

    drained(&collector, 3).await;

    assert_eq!(collector.count(EventKind::TaskStarting), 3);
    assert_eq!(collector.count(EventKind::TaskStopped), 3);
    assert_eq!(collector.count(EventKind::ActorExhausted), 3);
    assert_eq!(collector.count(EventKind::TaskRemoved), 3);

    for label in ["a", "b", "c"] {
        let evs = collector.by_label(label);
        assert!(
            evs.iter().any(|e| e.kind == EventKind::TaskStarting),
            "missing TaskStarting for {label}"
        );
        assert!(
            evs.iter().any(|e| e.kind == EventKind::ActorExhausted),
            "missing ActorExhausted for {label}"
        );
    }
}
