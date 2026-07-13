//! Task-default resolution across the main registry admission paths.

mod common;

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use common::{fast_backoff, with_timeout};
use taskvisor::prelude::*;

fn pending(name: &str) -> TaskRef {
    TaskFn::arc(name, |_ctx: TaskContext| async move {
        std::future::pending::<()>().await;
        Ok(())
    })
}

fn timeout_defaults() -> TaskDefaults {
    TaskDefaults::default().with_timeout(Duration::from_millis(20))
}

#[tokio::test(flavor = "current_thread")]
async fn dynamic_add_applies_inherited_timeout() {
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_task_defaults(timeout_defaults())
        .build();
    let handle = supervisor.serve();

    let (_, waiter) = handle
        .add_and_watch(TaskSpec::once(pending("dynamic-default-timeout")))
        .await
        .expect("dynamic task must be admitted");
    let outcome = with_timeout(2, waiter.wait())
        .await
        .expect("watched task must resolve");

    assert!(matches!(outcome, TaskOutcome::Failed { .. }));
    handle.shutdown().await.expect("shutdown must join");
}

#[tokio::test(flavor = "current_thread")]
async fn fully_inherited_spec_uses_default_restart_policy() {
    let runs = Arc::new(AtomicU32::new(0));
    let task_runs = Arc::clone(&runs);
    let task = TaskFn::arc("fully-inherited", move |_ctx: TaskContext| {
        let runs = Arc::clone(&task_runs);
        async move {
            runs.fetch_add(1, Ordering::SeqCst);
            Err(TaskError::fail("stop after one"))
        }
    });
    let defaults = TaskDefaults::default()
        .with_restart(RestartPolicy::Never)
        .with_backoff(fast_backoff());
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_task_defaults(defaults)
        .build();
    let handle = supervisor.serve();
    let (_, waiter) = handle
        .add_and_watch(TaskSpec::from_defaults(task))
        .await
        .expect("fully inherited task must be admitted");

    assert!(matches!(
        with_timeout(2, waiter.wait()).await,
        Ok(TaskOutcome::Failed { .. })
    ));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    handle.shutdown().await.expect("shutdown must join");
}

#[tokio::test(flavor = "current_thread")]
async fn static_batch_applies_inherited_timeout() {
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_task_defaults(timeout_defaults())
        .build();

    with_timeout(
        2,
        supervisor.run(vec![TaskSpec::once(pending("static-default-timeout"))]),
    )
    .await
    .expect("timed-out static task must reach natural completion");
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_none_disables_inherited_timeout() {
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_task_defaults(timeout_defaults())
        .build();
    let handle = supervisor.serve();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let released = Arc::new(std::sync::Mutex::new(Some(released)));
    let task = TaskFn::arc("explicit-no-timeout", move |_ctx: TaskContext| {
        let released = released
            .lock()
            .expect("release lock poisoned")
            .take()
            .expect("one-shot task runs once");
        async move {
            let _ = released.await;
            Ok(())
        }
    });

    let (_, waiter) = handle
        .add_and_watch(TaskSpec::once(task).with_timeout(None))
        .await
        .expect("task must be admitted");
    let mut outcome = Box::pin(waiter.wait());
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(1), &mut outcome)
            .await
            .is_err(),
        "explicit None must beat the inherited 20ms timeout"
    );
    release.send(()).expect("task is waiting for release");
    assert!(matches!(
        with_timeout(2, outcome).await,
        Ok(TaskOutcome::Completed)
    ));
    handle.shutdown().await.expect("shutdown must join");
}

#[tokio::test(flavor = "current_thread")]
async fn retry_default_and_explicit_unlimited_override_are_distinct() {
    let defaults = TaskDefaults::default()
        .with_backoff(fast_backoff())
        .with_max_retries(NonZeroU32::new(1));

    let limited_runs = Arc::new(AtomicU32::new(0));
    let runs = Arc::clone(&limited_runs);
    let limited = TaskFn::arc("inherited-retry-limit", move |_ctx: TaskContext| {
        let runs = Arc::clone(&runs);
        async move {
            runs.fetch_add(1, Ordering::SeqCst);
            Err(TaskError::fail("retry"))
        }
    });
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_task_defaults(defaults.clone())
        .build();
    let handle = supervisor.serve();
    let (_, waiter) = handle
        .add_and_watch(TaskSpec::restartable(limited))
        .await
        .expect("limited task must be admitted");
    assert!(matches!(
        with_timeout(2, waiter.wait()).await,
        Ok(TaskOutcome::Failed { .. })
    ));
    assert_eq!(limited_runs.load(Ordering::SeqCst), 2);
    handle.shutdown().await.expect("shutdown must join");

    let unlimited_runs = Arc::new(AtomicU32::new(0));
    let runs = Arc::clone(&unlimited_runs);
    let succeeds_on_third = TaskFn::arc("explicit-unlimited", move |_ctx: TaskContext| {
        let runs = Arc::clone(&runs);
        async move {
            if runs.fetch_add(1, Ordering::SeqCst) < 2 {
                Err(TaskError::fail("retry"))
            } else {
                Ok(())
            }
        }
    });
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_task_defaults(defaults)
        .build();
    let handle = supervisor.serve();
    let (_, waiter) = handle
        .add_and_watch(TaskSpec::restartable(succeeds_on_third).with_max_retries(None))
        .await
        .expect("unlimited task must be admitted");
    assert!(matches!(
        with_timeout(2, waiter.wait()).await,
        Ok(TaskOutcome::Completed)
    ));
    assert_eq!(unlimited_runs.load(Ordering::SeqCst), 3);
    handle.shutdown().await.expect("shutdown must join");
}

#[cfg(feature = "controller")]
#[tokio::test(flavor = "current_thread")]
async fn controller_admission_applies_inherited_timeout() {
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_task_defaults(timeout_defaults())
        .with_controller(ControllerConfig::default())
        .build();
    let handle = supervisor.serve();

    let (_, waiter) = handle
        .submit_and_watch(ControllerSpec::queue(TaskSpec::once(pending(
            "controller-default-timeout",
        ))))
        .await
        .expect("controller task must be admitted");
    assert!(matches!(
        with_timeout(2, waiter.wait()).await,
        Ok(TaskOutcome::Failed { .. })
    ));
    handle.shutdown().await.expect("shutdown must join");
}
