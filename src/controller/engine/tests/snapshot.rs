//! Tests for mapping internal slot phases to the public snapshot API.

use super::support::*;
use crate::controller::engine::Controller;
use crate::controller::engine::state::{AdmissionTransition, ReplaceAction, SlotState};

#[tokio::test]
async fn snapshot_maps_every_internal_slot_phase_and_owner() {
    use crate::controller::SlotStatusKind;

    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
    let admitting_id = TaskId::next();
    let cancel_pending_id = TaskId::next();
    let running_id = TaskId::next();
    let terminating_id = TaskId::next();
    let now = Instant::now();

    let mut admitting = SlotState::new();
    assert!(admitting.begin_admission(admitting_id, now - Duration::from_secs(4)));
    let mut cancel_pending = SlotState::new();
    assert!(cancel_pending.begin_admission(cancel_pending_id, now - Duration::from_secs(5)));
    assert_eq!(
        cancel_pending.request_replacement(now - Duration::from_secs(3)),
        ReplaceAction::WaitForAdmission
    );
    let mut running = SlotState::new();
    assert!(running.begin_admission(running_id, now - Duration::from_secs(5)));
    assert_eq!(
        running.confirm_admission(running_id, now - Duration::from_secs(2)),
        AdmissionTransition::Running
    );
    let mut terminating = SlotState::new();
    assert!(terminating.begin_admission(terminating_id, now - Duration::from_secs(5)));
    assert_eq!(
        terminating.confirm_admission(terminating_id, now - Duration::from_secs(4)),
        AdmissionTransition::Running
    );
    assert_eq!(
        terminating.request_replacement(now - Duration::from_secs(1)),
        ReplaceAction::RemoveNow(terminating_id)
    );

    let with_queue = |mut slot: SlotState, depth: usize| {
        for _ in 0..depth {
            slot.queue
                .push_back(pending(TaskId::next(), make_spec("snapshot-queued")));
        }
        slot
    };
    for (name, slot) in [
        ("terminating", with_queue(terminating, 4)),
        ("running", with_queue(running, 3)),
        ("idle", SlotState::new()),
        ("cancel-pending", with_queue(cancel_pending, 2)),
        ("admitting", with_queue(admitting, 1)),
    ] {
        ctrl.state()
            .slots
            .insert(Arc::from(name), Arc::new(Mutex::new(slot)));
    }

    let snap = ctrl.snapshot().await;
    assert_eq!(snap.len(), 5);
    assert_eq!(snap.total_queued(), 10);
    assert_eq!(snap.running_count(), 1);
    assert_eq!(
        snap.slots
            .iter()
            .map(|slot| slot.slot.as_ref())
            .collect::<Vec<_>>(),
        [
            "admitting",
            "cancel-pending",
            "idle",
            "running",
            "terminating"
        ]
    );

    for (name, status, owner, queue_depth, minimum_age) in [
        ("idle", SlotStatusKind::Idle, None, 0, Duration::ZERO),
        (
            "admitting",
            SlotStatusKind::Admitting,
            Some(admitting_id),
            1,
            Duration::from_secs(4),
        ),
        (
            "cancel-pending",
            SlotStatusKind::Terminating,
            Some(cancel_pending_id),
            2,
            Duration::from_secs(3),
        ),
        (
            "running",
            SlotStatusKind::Running,
            Some(running_id),
            3,
            Duration::from_secs(2),
        ),
        (
            "terminating",
            SlotStatusKind::Terminating,
            Some(terminating_id),
            4,
            Duration::from_secs(1),
        ),
    ] {
        let view = snap.slot(name).expect("the inserted slot must be visible");
        assert_eq!(view.status, status, "wrong public status for {name}");
        assert_eq!(view.owner_id, owner, "wrong phase-owned id for {name}");
        assert_eq!(
            view.queue_depth, queue_depth,
            "wrong queue depth for {name}"
        );
        if status == SlotStatusKind::Idle {
            assert_eq!(view.status_for, Duration::ZERO);
        } else {
            assert!(
                view.status_for >= minimum_age,
                "wrong status timestamp selected for {name}: {:?}",
                view.status_for
            );
        }
    }
}
