//! Graceful-shutdown & run-completion integration tests.
//!
//! Shutdown assertions target the **return value** (the reliable contract).

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use taskvisor::prelude::*;

fn make_stubborn(name: &str) -> TaskRef {
    TaskFn::arc(name, |_ctx: CancellationToken| async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok(())
    })
}

fn served(grace: Duration) -> SupervisorHandle {
    Supervisor::builder(SupervisorConfig {
        grace,
        ..Default::default()
    })
    .build()
    .serve()
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_cooperative_returns_ok() {
    let handle = served(Duration::from_secs(5));
    handle
        .add_and_wait(
            TaskSpec::restartable(make_coop("c1")),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    handle
        .add_and_wait(
            TaskSpec::restartable(make_coop("c2")),
            Duration::from_secs(1),
        )
        .await
        .unwrap();

    with_timeout(5, handle.shutdown())
        .await
        .expect("cooperative tasks drain within grace → Ok");
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_stubborn_under_small_grace_returns_grace_exceeded_force_aborts() {
    let handle = served(Duration::from_millis(200));
    handle
        .add_and_wait(
            TaskSpec::once(make_stubborn("stubborn")),
            Duration::from_secs(1),
        )
        .await
        .unwrap();

    match with_timeout(5, handle.shutdown()).await {
        Err(RuntimeError::GraceExceeded { grace, stuck }) => {
            assert_eq!(grace, Duration::from_millis(200));
            assert!(stuck.iter().any(|n| &**n == "stubborn"));
        }
        other => panic!("expected GraceExceeded, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_empty_registry_returns_ok() {
    let handle = served(SupervisorConfig::default().grace);
    with_timeout(5, handle.shutdown())
        .await
        .expect("empty registry drains instantly → Ok");
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_mixed_reports_only_stubborn_in_stuck() {
    let handle = served(Duration::from_millis(500));
    handle
        .add_and_wait(
            TaskSpec::restartable(make_coop("coop")),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    handle
        .add_and_wait(
            TaskSpec::once(make_stubborn("stuck")),
            Duration::from_secs(1),
        )
        .await
        .unwrap();

    match with_timeout(5, handle.shutdown()).await {
        Err(RuntimeError::GraceExceeded { stuck, .. }) => {
            assert!(stuck.iter().any(|n| &**n == "stuck"));
            assert!(
                !stuck.iter().any(|n| &**n == "coop"),
                "a cooperative task must not be reported as stuck"
            );
        }
        other => panic!("expected GraceExceeded, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_zero_grace_force_terminates_stubborn_immediately() {
    let handle = served(Duration::ZERO);
    handle
        .add_and_wait(TaskSpec::once(make_stubborn("z")), Duration::from_secs(1))
        .await
        .unwrap();

    match with_timeout(5, handle.shutdown()).await {
        Err(RuntimeError::GraceExceeded { grace, stuck }) => {
            assert_eq!(grace, Duration::ZERO);
            assert!(stuck.iter().any(|n| &**n == "z"));
        }
        other => panic!("expected GraceExceeded, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn run_returns_ok_when_all_oneshots_complete_and_registry_empties() {
    let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    let specs = vec![
        TaskSpec::once(make_ok_once("o1")),
        TaskSpec::once(make_ok_once("o2")),
        TaskSpec::once(make_ok_once("o3")),
    ];
    with_timeout(5, sup.run(specs))
        .await
        .expect("run returns Ok once registry empties");
}

#[tokio::test(flavor = "current_thread")]
async fn run_blocks_while_gated_task_alive_then_unblocks_on_completion() {
    let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    let gate = Arc::new(tokio::sync::Notify::new());

    let g = gate.clone();
    let task = TaskFn::arc("gated", move |_ctx: CancellationToken| {
        let g = g.clone();
        async move {
            g.notified().await;
            Ok(())
        }
    });

    let sup2 = sup.clone();
    let jh = tokio::spawn(async move { sup2.run(vec![TaskSpec::once(task)]).await });

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!jh.is_finished(), "run() must block while a task is alive");

    gate.notify_one();
    with_timeout(5, jh)
        .await
        .expect("run() task should not panic")
        .expect("run returns Ok after the gated task completes");
}
