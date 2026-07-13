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
        .wait_for(EventKind::ActorExhausted, Duration::from_secs(2))
        .await
        .expect("ActorExhausted was not observed");
    collector
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn per_attempt_timeout_emits_timeout_hit_before_task_failed_then_retries() {
    let task = TaskFn::arc("slow", |_ctx: TaskContext| async move {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok(())
    });
    let spec = TaskSpec::restartable(task)
        .with_timeout(Duration::from_millis(50))
        .with_backoff(fast_backoff())
        .with_max_retries(NonZeroU32::new(1).unwrap());
    let collector = run_to_exhaustion(spec).await;

    assert_eq!(collector.count(EventKind::TimeoutHit), 2);
    assert_eq!(collector.count(EventKind::TaskFailed), 2);
    assert_eq!(collector.count(EventKind::BackoffScheduled), 1);
    assert_eq!(collector.count(EventKind::ActorExhausted), 1);

    for attempt in 1..=2u32 {
        let hit = collector
            .find_all(EventKind::TimeoutHit)
            .into_iter()
            .find(|e| e.attempt == Some(attempt))
            .unwrap();
        let failed = collector
            .find_all(EventKind::TaskFailed)
            .into_iter()
            .find(|e| e.attempt == Some(attempt))
            .unwrap();
        assert!(hit.seq < failed.seq, "TimeoutHit must precede TaskFailed");
        assert!(failed.reason.as_deref().unwrap().contains("timed out"));
        assert_eq!(failed.exit_code, None);
    }

    let reason = collector
        .find(EventKind::ActorExhausted)
        .unwrap()
        .reason
        .unwrap();
    assert!(reason.contains("max_retries_exceeded") && reason.contains("(1/1)"));
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

    assert_eq!(collector.count(EventKind::TimeoutHit), 1);
    assert_eq!(collector.count(EventKind::TaskFailed), 1);
    assert_eq!(collector.count(EventKind::BackoffScheduled), 1);
    assert_eq!(collector.count(EventKind::TaskStarting), 2);
    assert_eq!(collector.count(EventKind::TaskStopped), 1);
    assert_eq!(collector.count(EventKind::ActorExhausted), 1);
    assert_eq!(collector.count(EventKind::ActorDead), 0);

    assert_eq!(
        collector
            .find(EventKind::BackoffScheduled)
            .unwrap()
            .backoff_source,
        Some(BackoffSource::Failure)
    );
    assert_eq!(
        collector
            .find(EventKind::ActorExhausted)
            .unwrap()
            .reason
            .as_deref(),
        Some("policy_exhausted_success")
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn zero_timeout_means_no_timeout_task_runs_to_completion() {
    let task = TaskFn::arc("zero-to", |_ctx: TaskContext| async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        Ok(())
    });
    let spec = TaskSpec::once(task).with_timeout(Duration::ZERO);
    let collector = run_to_exhaustion(spec).await;

    assert_eq!(collector.count(EventKind::TimeoutHit), 0);
    assert_eq!(collector.count(EventKind::TaskStopped), 1);
    assert_eq!(collector.count(EventKind::TaskFailed), 0);
    assert_eq!(
        collector
            .find(EventKind::ActorExhausted)
            .unwrap()
            .reason
            .as_deref(),
        Some("policy_exhausted_success")
    );
}
