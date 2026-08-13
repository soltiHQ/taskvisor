//! Tests for submission intake through the controller handle.

use super::support::*;

#[tokio::test]
async fn try_submit_and_watch_is_fail_fast_and_preserves_watched_outcome() {
    let config = ControllerConfig::default().with_queue_capacity(NonZeroUsize::new(1).unwrap());
    let ctrl = make_controller(config, Bus::new(64));

    let task: TaskRef = TaskFn::arc(|_ctx: TaskContext| async { Ok(()) });
    let (_id, waiter) = ctrl
        .handle()
        .try_submit_and_watch(
            ControllerSpec::queue(TaskSpec::once("try-watched", task)).with_slot("s"),
        )
        .expect("the watched submission must occupy the only command slot");
    assert!(matches!(
        ctrl.handle().try_submit_and_watch(
            ControllerSpec::queue(waiting_spec("try-watched-overflow")).with_slot("s")
        ),
        Err(ControllerError::Full)
    ));

    let mut rx = ctrl.rx.write().await.take().expect("rx present");
    ctrl.finalize_pending_on_shutdown(&mut rx).await;
    drop(rx);

    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), waiter).await,
        Ok(Ok(TaskOutcome::Rejected { .. }))
    ));
}

#[tokio::test]
async fn ownership_wait_returns_closed_when_controller_receiver_closes() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let source = crate::core::deferred_drop::TestReservationSource::new(1);
    let held = source
        .try_reserve()
        .expect("the isolated ownership slot starts available");
    let handle = ctrl.handle().with_reservation_source(source.clone());
    let mut submission = Box::pin(
        handle.submit(
            ControllerSpec::queue(waiting_spec("ownership-close-waiter"))
                .with_slot("ownership-close-slot"),
        ),
    );

    std::future::poll_fn(|context| match submission.as_mut().poll(context) {
        std::task::Poll::Pending => std::task::Poll::Ready(()),
        std::task::Poll::Ready(result) => {
            panic!("ownership-saturated submission must initially wait, got {result:?}")
        }
    })
    .await;

    let receiver = ctrl.rx.write().await.take().expect("receiver present");
    drop(receiver);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), submission).await,
        Ok(Err(ControllerError::Closed))
    ));

    drop(held);
    assert!(
        source.try_reserve().is_ok(),
        "canceling the ownership wait must remove its semaphore waiter"
    );
}

#[test]
fn try_submit_reports_lazy_start_failure_without_enqueuing_and_exact_retry_succeeds() {
    let config = ControllerConfig::default();
    let queue_capacity = config.queue_capacity().get();
    let lazy = crate::core::deferred_drop::TestLazyDomain::fail_first_start_at_worker(64, 1);
    let domain = lazy.domain();
    let ctrl = make_controller_with_domain(config, Bus::new(64), domain.clone());
    let handle = ctrl.handle();
    let spawn_calls = Arc::new(AtomicUsize::new(0));
    let spec = spawn_counting_controller_spec("lazy-try-submit", &spawn_calls);

    assert!(!domain.is_started());
    let error = handle
        .try_submit(spec.clone())
        .expect_err("the injected first core start must fail");
    assert_lazy_start_failure(error, 1);
    assert!(!domain.is_started(), "a partial core set must not publish");
    assert_eq!(lazy.spawn_calls(), 2);
    assert_eq!(ctrl.tx.capacity(), queue_capacity);
    assert_eq!(spawn_calls.load(Ordering::Acquire), 0);

    handle
        .try_submit(spec)
        .expect("the same dormant domain must support an exact retry");
    assert!(domain.is_started());
    assert_eq!(lazy.spawn_calls(), 5);
    assert_eq!(ctrl.tx.capacity(), queue_capacity - 1);
    assert_eq!(spawn_calls.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn submit_reports_lazy_start_failure_without_enqueuing_and_exact_retry_succeeds() {
    let config = ControllerConfig::default();
    let queue_capacity = config.queue_capacity().get();
    let lazy = crate::core::deferred_drop::TestLazyDomain::fail_first_start_at_worker(64, 2);
    let domain = lazy.domain();
    let ctrl = make_controller_with_domain(config, Bus::new(64), domain.clone());
    let handle = ctrl.handle();
    let spawn_calls = Arc::new(AtomicUsize::new(0));
    let spec = spawn_counting_controller_spec("lazy-submit", &spawn_calls);

    assert!(!domain.is_started());
    let error = handle
        .submit(spec.clone())
        .await
        .expect_err("the injected first core start must fail");
    assert_lazy_start_failure(error, 2);
    assert!(!domain.is_started(), "a partial core set must not publish");
    assert_eq!(lazy.spawn_calls(), 3);
    assert_eq!(ctrl.tx.capacity(), queue_capacity);
    assert_eq!(spawn_calls.load(Ordering::Acquire), 0);

    handle
        .submit(spec)
        .await
        .expect("the same dormant domain must support an exact retry");
    assert!(domain.is_started());
    assert_eq!(lazy.spawn_calls(), 6);
    assert_eq!(ctrl.tx.capacity(), queue_capacity - 1);
    assert_eq!(spawn_calls.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn minimum_queue_capacity_is_supported() {
    let sup = Supervisor::builder(crate::SupervisorConfig::default())
        .with_controller(
            ControllerConfig::default()
                .with_queue_capacity(NonZeroUsize::new(1).unwrap())
                .with_max_slot_queue(1),
        )
        .build();
    let handle = sup.serve().expect("runtime startup");

    let task: TaskRef = TaskFn::arc(|_ctx: TaskContext| async { Ok(()) });
    handle
        .submit(ControllerSpec::queue(TaskSpec::once(
            "minimum-capacity",
            task,
        )))
        .await
        .expect("submission must work with the minimum non-zero capacity");

    let _ = handle.shutdown().await;
}
