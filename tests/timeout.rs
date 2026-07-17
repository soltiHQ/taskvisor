//! Per-attempt timeout integration tests.

mod common;

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use common::*;
use taskvisor::BackoffSource;
use taskvisor::prelude::*;

async fn run_to_exhaustion(spec: TaskSpec) -> Arc<EventCollector> {
    let (supervisor, collector) = supervisor_with_collector(SupervisorConfig::default());
    with_timeout(10, supervisor.run(vec![spec]))
        .await
        .expect("run() should return Ok");
    collector
        .wait_for(EventKind::TaskFinished, Duration::from_secs(2))
        .await
        .expect("TaskFinished was not observed");
    collector
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn configured_timeout_emits_one_terminal_attempt_event_then_retries() {
    let task = TaskFn::arc("slow", |_ctx: TaskContext| async move {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok(())
    });
    let spec = TaskSpec::restartable(task)
        .with_timeout(Duration::from_millis(50))
        .with_backoff(fast_backoff())
        .with_max_retries(NonZeroU32::new(1).unwrap());
    let collector = run_to_exhaustion(spec).await;

    assert_eq!(collector.count(EventKind::AttemptTimedOut), 2);
    assert_eq!(collector.count(EventKind::AttemptFailed), 0);
    assert_eq!(collector.count(EventKind::BackoffScheduled), 1);
    assert_eq!(collector.count(EventKind::TaskFinished), 1);

    for attempt in 1..=2u32 {
        let hit = collector
            .find_all(EventKind::AttemptTimedOut)
            .into_iter()
            .find(|e| e.attempt == Some(attempt))
            .unwrap();
        assert_eq!(hit.timeout_ms, Some(50));
    }

    let finished = collector.find(EventKind::TaskFinished).unwrap();
    assert_eq!(finished.outcome_kind, Some(TaskOutcomeKind::Failed));
    let reason = finished.reason.unwrap();
    assert!(reason.contains("timed out"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn timeout_then_success_unlimited_retries_exhausts_on_success() {
    let n = Arc::new(AtomicU32::new(0));
    let nc = n.clone();
    let task = TaskFn::arc("slow-then-ok", move |_ctx: TaskContext| {
        let nc = nc.clone();
        async move {
            let c = nc.fetch_add(1, Ordering::SeqCst);
            if c == 0 {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
            Ok(())
        }
    });
    let spec = TaskSpec::restartable(task)
        .with_timeout(Duration::from_millis(50))
        .with_backoff(fast_backoff());
    let collector = run_to_exhaustion(spec).await;

    assert_eq!(collector.count(EventKind::AttemptTimedOut), 1);
    assert_eq!(collector.count(EventKind::AttemptFailed), 0);
    assert_eq!(collector.count(EventKind::BackoffScheduled), 1);
    assert_eq!(collector.count(EventKind::AttemptStarting), 2);
    assert_eq!(collector.count(EventKind::AttemptSucceeded), 1);
    assert_eq!(collector.count(EventKind::TaskFinished), 1);

    assert_eq!(
        collector
            .find(EventKind::BackoffScheduled)
            .unwrap()
            .backoff_source,
        Some(BackoffSource::Failure)
    );
    let finished = collector.find(EventKind::TaskFinished).unwrap();
    assert_eq!(finished.outcome_kind, Some(TaskOutcomeKind::Completed));
    assert_eq!(finished.reason, None);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn zero_timeout_means_no_timeout_task_runs_to_completion() {
    let task = TaskFn::arc("zero-to", |_ctx: TaskContext| async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        Ok(())
    });
    let spec = TaskSpec::once(task).with_timeout(Duration::ZERO);
    let collector = run_to_exhaustion(spec).await;

    assert_eq!(collector.count(EventKind::AttemptTimedOut), 0);
    assert_eq!(collector.count(EventKind::AttemptSucceeded), 1);
    assert_eq!(collector.count(EventKind::AttemptFailed), 0);
    let finished = collector.find(EventKind::TaskFinished).unwrap();
    assert_eq!(finished.outcome_kind, Some(TaskOutcomeKind::Completed));
    assert_eq!(finished.reason, None);
}

#[tokio::test(flavor = "current_thread")]
async fn task_returned_timeout_is_an_attempt_failure_not_a_configured_deadline() {
    let task = TaskFn::arc("reported-timeout", |_ctx: TaskContext| async move {
        Err(TaskError::timeout(Duration::from_secs(7)))
    });
    let collector = run_to_exhaustion(TaskSpec::once(task)).await;

    assert_eq!(collector.count(EventKind::AttemptTimedOut), 0);
    assert_eq!(collector.count(EventKind::AttemptFailed), 1);
    assert_eq!(
        collector
            .find(EventKind::TaskFinished)
            .unwrap()
            .outcome_kind,
        Some(TaskOutcomeKind::Failed)
    );
}
