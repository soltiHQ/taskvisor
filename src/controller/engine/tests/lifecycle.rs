//! Tests for controller loop progress, panic isolation, and shared joining.

use super::support::*;
use crate::controller::engine::{Controller, ControllerTask};

#[tokio::test(flavor = "current_thread")]
async fn controller_task_join_can_resume_after_a_dropped_waiter() {
    let (release, released) = oneshot::channel::<()>();
    let task = Arc::new(ControllerTask::new(tokio::spawn(async move {
        let _ = released.await;
    })));
    let bus = Bus::new(8);

    let first_task = Arc::clone(&task);
    let first_bus = bus.clone();
    let first = tokio::spawn(async move { first_task.join(&first_bus).await });
    assert!(
        poll_until(Duration::from_secs(1), || async { task.state_is_locked() }).await,
        "the first waiter must own the shared join state"
    );
    first.abort();
    let _ = first.await;
    assert!(
        poll_until(Duration::from_secs(1), || async { !task.state_is_locked() }).await,
        "aborting the first waiter must release the shared join state"
    );

    let second_task = Arc::clone(&task);
    let second_bus = bus.clone();
    let second = tokio::spawn(async move { second_task.join(&second_bus).await });
    assert!(
        poll_until(Duration::from_secs(1), || async { task.state_is_locked() }).await,
        "the second waiter must resume ownership of the stored JoinHandle"
    );
    assert!(
        !second.is_finished(),
        "the stored JoinHandle must remain pending after the first waiter is dropped"
    );

    release.send(()).expect("the controller task is waiting");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), second).await,
        Ok(Ok(true))
    ));
    assert!(task.is_joined().await);
}

#[tokio::test]
async fn guarded_converts_panic_to_diagnostic_and_survives() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let mut rx = ctrl.bus.subscribe();

    let _ = ctrl.guarded("unit", async { panic!("boom {}", 1) }).await;

    let ev = rx
        .try_recv()
        .expect("a panicking work-unit must publish a diagnostic");
    assert_eq!(ev.kind, EventKind::RuntimeFailure);
    assert!(
        ev.reason.as_deref().unwrap_or_default().contains("boom 1"),
        "diagnostic must carry the panic message, got {:?}",
        ev.reason
    );
}

#[tokio::test(flavor = "current_thread")]
async fn natural_run_waits_for_controller_join() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
    sup.core().attach_controller(&ctrl);
    ctrl.run();

    let slot = ctrl.get_or_create_slot("blocked-natural-slot");
    let slot_guard = slot.lock().await;
    ctrl.handle()
        .submit(
            ControllerSpec::queue(waiting_spec("blocked-natural-task"))
                .with_slot("blocked-natural-slot"),
        )
        .await
        .expect("the blocking submission must enter controller intake");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            ctrl.tx.capacity() == ctrl.config.queue_capacity().get()
        })
        .await,
        "the controller must block on the held slot before natural shutdown"
    );

    let run_sup = Arc::clone(&sup);
    let run = tokio::spawn(async move { run_sup.run(vec![]).await });
    assert!(
        poll_until(Duration::from_secs(2), || async {
            ctrl.task.get().is_some_and(ControllerTask::state_is_locked)
        })
        .await,
        "natural shutdown must reach the shared controller join"
    );
    assert!(
        !run.is_finished(),
        "natural run must not return while the controller loop is blocked"
    );

    drop(slot_guard);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), run).await,
        Ok(Ok(Ok(())))
    ));
    assert!(ctrl.is_joined().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completed_owner_progresses_under_continuously_ready_intake() {
    let sup = Supervisor::builder(crate::SupervisorConfig::default())
        .with_controller(
            ControllerConfig::default().with_queue_capacity(NonZeroUsize::new(1).unwrap()),
        )
        .build();
    let handle = sup.serve().expect("runtime startup");

    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let owner: TaskRef = TaskFn::arc({
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        move |_ctx| {
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            async move {
                started.notify_one();
                release.notified().await;
                Ok(())
            }
        }
    });
    let (owner_id, owner_waiter) = handle
        .submit_and_watch(
            ControllerSpec::queue(TaskSpec::once("starvation-owner", owner)).with_slot("hot-slot"),
        )
        .await
        .expect("the initial owner submission must enter the controller");
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("the initial owner must start");

    let flood_task: TaskRef = TaskFn::arc(|ctx| async move {
        ctx.cancelled().await;
        Ok(())
    });
    let flood_spec =
        ControllerSpec::drop_if_running(TaskSpec::once("starvation-flood", flood_task))
            .with_slot("hot-slot");
    let stop = Arc::new(AtomicBool::new(false));
    let saw_full = Arc::new(AtomicBool::new(false));
    let producer_failed = Arc::new(AtomicBool::new(false));
    let mut producers = Vec::new();

    for _ in 0..4 {
        let producer_handle = handle.clone();
        let producer_spec = flood_spec.clone();
        let producer_stop = Arc::clone(&stop);
        let producer_saw_full = Arc::clone(&saw_full);
        let producer_failed = Arc::clone(&producer_failed);
        producers.push(std::thread::spawn(move || {
            while !producer_stop.load(Ordering::Relaxed) {
                match producer_handle.try_submit(producer_spec.clone()) {
                    Ok(_) => {}
                    Err(ControllerError::Full) => {
                        producer_saw_full.store(true, Ordering::Release);
                        std::hint::spin_loop();
                    }
                    Err(ControllerError::ResourceLimit { .. }) => {
                        std::hint::spin_loop();
                    }
                    Err(_) => {
                        producer_failed.store(true, Ordering::Release);
                        break;
                    }
                }
            }
        }));
    }

    let saturated = tokio::time::timeout(Duration::from_secs(2), async {
        while !saw_full.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();

    release.notify_one();
    let owner_outcome = tokio::time::timeout(Duration::from_secs(2), owner_waiter.wait()).await;
    let progressed = poll_until(Duration::from_secs(2), || async {
        let Some(snapshot) = handle.controller_snapshot().await else {
            return false;
        };
        snapshot
            .slot("hot-slot")
            .is_none_or(|slot| slot.owner_id != Some(owner_id))
    })
    .await;

    stop.store(true, Ordering::Release);
    let producers_joined = producers
        .into_iter()
        .all(|producer| producer.join().is_ok());
    let shutdown = handle.shutdown().await;

    assert!(
        saturated,
        "the producers must keep the command channel ready"
    );
    assert!(
        matches!(owner_outcome, Ok(Ok(TaskOutcome::Completed))),
        "the initial owner must complete normally"
    );
    assert!(
        progressed,
        "a ready completion result must advance the slot while intake remains saturated"
    );
    assert!(producers_joined, "all intake producers must exit cleanly");
    assert!(
        !producer_failed.load(Ordering::Acquire),
        "the controller must remain open during the saturation phase"
    );
    assert!(shutdown.is_ok(), "the supervisor must shut down cleanly");
}
