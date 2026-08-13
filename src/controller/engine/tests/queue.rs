//! Tests for queue limits, reverse indexes, and head replacement.

use super::support::*;
use crate::controller::engine::state::SlotState;

#[test]
fn queue_full_reason_respects_the_capacity_boundary() {
    let config = ControllerConfig::new(NonZeroUsize::new(16).unwrap(), 3);
    let ctrl = make_controller(config, Bus::new(64));

    for (depth, expected_rejection) in [(0, false), (2, false), (3, true), (10, true)] {
        assert_eq!(
            ctrl.queue_full_reason(depth).is_some(),
            expected_rejection,
            "unexpected decision at queue depth {depth}"
        );
    }
}

#[test]
fn queued_reverse_index_tracks_push_pop_and_position_removal() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let slot_name = slot_arc_name();
    let mut slot = SlotState::new();
    let first = TaskId::next();
    let second = TaskId::next();

    ctrl.push_queued(&mut slot, &slot_name, pending(first, make_spec("first")));
    ctrl.push_queued(&mut slot, &slot_name, pending(second, make_spec("second")));
    assert_eq!(
        ctrl.state().queued_slots.get(&first).cloned(),
        Some(Arc::clone(&slot_name))
    );
    assert_eq!(
        ctrl.state().queued_slots.get(&second).cloned(),
        Some(Arc::clone(&slot_name))
    );

    assert_eq!(
        ctrl.pop_queued_front(&mut slot).map(|pending| pending.id),
        Some(first)
    );
    assert!(!ctrl.state().queued_slots.contains_key(&first));
    assert_eq!(
        ctrl.remove_queued_at(&mut slot, 0)
            .map(|pending| pending.id),
        Some(second)
    );
    assert!(!ctrl.state().queued_slots.contains_key(&second));
    assert!(slot.queue.is_empty());
}

#[test]
fn aggregate_pending_budget_bounds_push_but_allows_head_replacement() {
    let config = ControllerConfig::default().with_max_total_pending(NonZeroUsize::new(1));
    let ctrl = make_controller(config, Bus::new(64));
    let slot_name = slot_arc_name();
    let mut slot = SlotState::new();
    let first = TaskId::next();
    let rejected = TaskId::next();
    let replacement = TaskId::next();

    assert!(
        ctrl.try_push_queued(&mut slot, &slot_name, pending(first, make_spec("first")))
            .is_ok()
    );
    let rejected = ctrl
        .try_push_queued(
            &mut slot,
            &slot_name,
            pending(rejected, make_spec("rejected")),
        )
        .expect_err("a second pending identity must exceed the global budget");
    assert_eq!(rejected.task_spec().name(), "rejected");

    let displaced = match ctrl.try_replace_head_or_push(
        &mut slot,
        &slot_name,
        pending(replacement, make_spec("replacement")),
    ) {
        Ok(displaced) => displaced,
        Err(_) => panic!("replacing a pending identity must not increase aggregate depth"),
    }
    .expect("the old head must be returned");
    assert_eq!(displaced.id, first);
    assert_eq!(slot.queue.front().map(|item| item.id), Some(replacement));
    assert_eq!(ctrl.state().queued_slots.len(), 1);
}
