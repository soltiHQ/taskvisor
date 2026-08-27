//! Direct-await shorthand for default management operations.

use taskvisor::prelude::*;

fn immediate_task(name: &str) -> TaskSpec {
    TaskSpec::once(name, TaskFn::arc(|_ctx| async { Ok(()) }))
}

fn held_task(name: &str) -> TaskSpec {
    TaskSpec::once(
        name,
        TaskFn::arc(|ctx| async move {
            ctx.cancelled().await;
            Ok(())
        }),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_add_operation_is_directly_awaitable_and_send() {
    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    let handle = supervisor.serve().expect("runtime startup");
    let submitting_handle = handle.clone();

    let id = tokio::spawn(async move {
        let result: Result<TaskId, RuntimeError> = submitting_handle
            .add(immediate_task("direct-await-add"))
            .await;
        result
    })
    .await
    .expect("add caller must join")
    .expect("default add must be admitted");

    assert!(id.get() > 0);
    handle.shutdown().await.expect("shutdown must join");
}

#[tokio::test(flavor = "current_thread")]
async fn default_add_await_preserves_registry_error() {
    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    let handle = supervisor.serve().expect("runtime startup");

    handle
        .add(held_task("direct-await-duplicate"))
        .await
        .expect("first add must be admitted");
    let result = handle.add(immediate_task("direct-await-duplicate")).await;

    assert!(matches!(
        result,
        Err(RuntimeError::TaskAlreadyExists { .. })
    ));
    handle.shutdown().await.expect("shutdown must join");
}

#[cfg(feature = "controller")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_submit_operation_is_directly_awaitable_and_send() {
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_controller(ControllerConfig::default())
        .build();
    let handle = supervisor.serve().expect("runtime startup");
    let submitting_handle = handle.clone();

    let id = tokio::spawn(async move {
        let result: Result<TaskId, ControllerError> = submitting_handle
            .submit(ControllerSpec::queue(immediate_task("direct-await-submit")))
            .await;
        result
    })
    .await
    .expect("submit caller must join")
    .expect("default submission must enter controller intake");

    assert!(id.get() > 0);
    handle.shutdown().await.expect("shutdown must join");
}

#[cfg(feature = "controller")]
#[tokio::test(flavor = "current_thread")]
async fn prepared_default_submit_is_directly_awaitable() {
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_controller(ControllerConfig::default())
        .build();
    let handle = supervisor.serve().expect("runtime startup");
    let prepared = handle
        .prepare_submission(ControllerSpec::queue(immediate_task(
            "direct-await-prepared-submit",
        )))
        .expect("controller is configured");
    let reserved_id = prepared.id();

    let submitted_id = prepared
        .submit()
        .await
        .expect("prepared submission must enter controller intake");

    assert_eq!(submitted_id, reserved_id);
    handle.shutdown().await.expect("shutdown must join");
}

#[cfg(feature = "controller")]
#[tokio::test(flavor = "current_thread")]
async fn default_submit_await_preserves_not_configured_error() {
    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    let handle = supervisor.serve().expect("runtime startup");

    let result = handle
        .submit(ControllerSpec::queue(immediate_task(
            "direct-await-without-controller",
        )))
        .await;

    assert!(matches!(result, Err(ControllerError::NotConfigured)));
    handle.shutdown().await.expect("shutdown must join");
}
