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

    let mut rx = ctrl.take_command_receiver().expect("rx present");
    ctrl.finalize_pending_on_shutdown(&mut rx).await;
    drop(rx);

    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), waiter).await,
        Ok(Ok(TaskOutcome::Rejected { .. }))
    ));
}

#[tokio::test]
async fn closed_channel_wins_before_submission_reservation() {
    let bus = Bus::new(64);
    let mut events = bus.subscribe();
    let ctrl = make_controller(ControllerConfig::default(), bus);
    let source = crate::core::deferred_drop::TestReservationSource::new(1);
    let id = TaskId::next();
    let handle = ctrl.handle().with_reservation_source(source.clone());

    let mut rx = ctrl.take_command_receiver().expect("receiver present");
    ctrl.finalize_pending_on_shutdown(&mut rx).await;

    let result = handle.try_submit_prepared_and_watch(
        id,
        ControllerSpec::queue(waiting_spec("closed-before-reservation"))
            .with_slot("closed-before-reservation-slot"),
    );
    assert!(matches!(result, Err(ControllerError::Closed)));
    assert_eq!(rx.len(), 0);
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
    assert!(
        drain_events(&mut events)
            .iter()
            .all(|event| event.id != Some(id)),
        "a submission rejected before reservation must not publish a controller event"
    );

    assert!(
        source.try_reserve().is_ok(),
        "channel closure must win before ownership is reserved"
    );
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

    let receiver = ctrl.take_command_receiver().expect("receiver present");
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

#[tokio::test]
async fn ownership_timeout_removes_waiter_without_controller_intake() {
    let config = ControllerConfig::default();
    let queue_capacity = config.queue_capacity().get();
    let ctrl = make_controller(config, Bus::new(64));
    let source = crate::core::deferred_drop::TestReservationSource::new(1);
    let held = source
        .try_reserve()
        .expect("the isolated ownership slot starts available");
    let handle = ctrl.handle().with_reservation_source(source.clone());
    let id = TaskId::next();

    let error = handle
        .submit_prepared_with_ownership_timeout(
            id,
            ControllerSpec::queue(waiting_spec("ownership-timeout"))
                .with_slot("ownership-timeout-slot"),
            Duration::ZERO,
        )
        .await
        .expect_err("saturated ownership admission must time out");

    assert_eq!(
        error,
        ControllerError::OwnershipAdmissionTimeout {
            timeout: Duration::ZERO,
        }
    );
    let snapshot = source.domain().snapshot(true);
    assert_eq!(snapshot.waiters, 0);
    assert_eq!(snapshot.available, Some(0));
    assert_eq!(ctrl.tx.capacity(), queue_capacity);

    drop(held);
    assert!(
        source.try_reserve().is_ok(),
        "the timed-out waiter must not retain a later ownership grant"
    );
}

#[tokio::test(start_paused = true)]
async fn positive_controller_ownership_deadline_expires_and_release_before_retry_succeeds() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let source = crate::core::deferred_drop::TestReservationSource::new(1);
    let domain = source.domain();
    let held = source
        .try_reserve()
        .expect("the isolated ownership slot starts available");
    let handle = ctrl.handle().with_reservation_source(source);
    let wait_for = Duration::from_secs(5);

    let mut expiring = Box::pin(
        handle.submit_prepared_with_ownership_timeout(
            TaskId::next(),
            ControllerSpec::queue(waiting_spec("positive-controller-ownership-timeout"))
                .with_slot("positive-controller-ownership-timeout-slot"),
            wait_for,
        ),
    );
    std::future::poll_fn(|context| match expiring.as_mut().poll(context) {
        std::task::Poll::Pending => std::task::Poll::Ready(()),
        std::task::Poll::Ready(result) => {
            panic!("saturated ownership admission must initially wait, got {result:?}")
        }
    })
    .await;
    assert_eq!(domain.snapshot(true).waiters, 1);

    tokio::time::advance(wait_for).await;
    assert!(matches!(
        expiring.await,
        Err(ControllerError::OwnershipAdmissionTimeout { timeout, .. })
            if timeout == wait_for
    ));
    assert_eq!(domain.snapshot(true).waiters, 0);

    let retry_id = TaskId::next();
    let mut admitted = Box::pin(
        handle.submit_prepared_with_ownership_timeout(
            retry_id,
            ControllerSpec::queue(waiting_spec("controller-release-before-deadline"))
                .with_slot("controller-release-before-deadline-slot"),
            wait_for,
        ),
    );
    std::future::poll_fn(|context| match admitted.as_mut().poll(context) {
        std::task::Poll::Pending => std::task::Poll::Ready(()),
        std::task::Poll::Ready(result) => {
            panic!("saturated ownership admission must initially wait, got {result:?}")
        }
    })
    .await;
    assert_eq!(domain.snapshot(true).waiters, 1);

    drop(held);
    assert_eq!(
        admitted
            .await
            .expect("releasing ownership before the deadline must admit the submission"),
        retry_id
    );

    let mut receiver = ctrl.take_command_receiver().expect("receiver present");
    drop(
        receiver
            .recv()
            .await
            .expect("the admitted command must be queued"),
    );
    drop(receiver);
}

#[tokio::test]
async fn controller_close_wins_when_ownership_deadline_is_also_ready() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let source = crate::core::deferred_drop::TestReservationSource::new(1);
    let _held = source
        .try_reserve()
        .expect("the isolated ownership slot starts available");
    let handle = ctrl.handle().with_reservation_source(source);
    let receiver = ctrl.take_command_receiver().expect("receiver present");
    drop(receiver);

    let error = handle
        .submit_prepared_with_ownership_timeout(
            TaskId::next(),
            ControllerSpec::queue(waiting_spec("ownership-timeout-closed"))
                .with_slot("ownership-timeout-closed-slot"),
            Duration::ZERO,
        )
        .await
        .expect_err("a closed controller must reject intake");

    assert_eq!(error, ControllerError::Closed);
}

#[tokio::test]
async fn ownership_timeout_stops_before_controller_queue_wait() {
    let config = ControllerConfig::default().with_queue_capacity(NonZeroUsize::new(1).unwrap());
    let ctrl = make_controller(config, Bus::new(64));
    let source = crate::core::deferred_drop::TestReservationSource::new(2);
    let handle = ctrl.handle().with_reservation_source(source);

    handle
        .try_submit(
            ControllerSpec::queue(waiting_spec("ownership-timeout-queue-blocker"))
                .with_slot("ownership-timeout-queue-blocker-slot"),
        )
        .expect("the first submission must fill the controller queue");

    let mut submission = Box::pin(
        handle.submit_prepared_with_ownership_timeout(
            TaskId::next(),
            ControllerSpec::queue(waiting_spec("ownership-timeout-after-permit"))
                .with_slot("ownership-timeout-after-permit-slot"),
            Duration::ZERO,
        ),
    );
    std::future::poll_fn(|context| match submission.as_mut().poll(context) {
        std::task::Poll::Pending => std::task::Poll::Ready(()),
        std::task::Poll::Ready(result) => {
            panic!("a full controller queue must keep the submission pending, got {result:?}")
        }
    })
    .await;

    let mut receiver = ctrl.take_command_receiver().expect("receiver present");
    drop(
        receiver
            .recv()
            .await
            .expect("the blocking command is queued"),
    );
    submission
        .await
        .expect("the ownership deadline must not cover controller queue capacity");
    drop(
        receiver
            .recv()
            .await
            .expect("the timed submission is queued"),
    );
    drop(receiver);
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
