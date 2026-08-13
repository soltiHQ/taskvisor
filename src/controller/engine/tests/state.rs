//! Tests for slot allocation and aggregate state limits.

use super::support::*;
use crate::controller::engine::state::SlotPhase;

#[test]
fn aggregate_slot_budget_allows_existing_identity_and_reclaims_idle_slot() {
    let config = ControllerConfig::default().with_max_controller_slots(NonZeroUsize::new(1));
    let ctrl = make_controller(config, Bus::new(64));
    let first_name: Arc<str> = Arc::from("first");
    let second_name: Arc<str> = Arc::from("second");

    let first = ctrl
        .try_get_or_create_slot(&first_name)
        .expect("the first slot must fit");
    let same = ctrl
        .try_get_or_create_slot(&first_name)
        .expect("an existing slot does not consume capacity");
    assert!(Arc::ptr_eq(&first, &same));
    assert!(matches!(ctrl.try_get_or_create_slot(&second_name), Err(1)));

    ctrl.gc_if_idle(&first_name, first.blocking_lock());
    assert!(ctrl.try_get_or_create_slot(&second_name).is_ok());
}

#[test]
fn try_get_or_create_slot_keeps_the_callers_arc_as_the_map_key() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let slot_name: Arc<str> = Arc::from("canonical-slot");

    ctrl.try_get_or_create_slot(&slot_name)
        .expect("the slot must fit");

    let state = ctrl.state();
    let stored_name = state
        .slots
        .keys()
        .find(|name| name.as_ref() == slot_name.as_ref())
        .expect("the slot map must contain the inserted name");
    assert!(Arc::ptr_eq(&slot_name, stored_name));
}

#[test]
fn get_or_create_slot_preserves_name_identity_and_initial_state() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));

    let slot_arc = ctrl.get_or_create_slot("my-slot");
    {
        let slot = slot_arc.blocking_lock();
        assert_eq!(slot.phase(), SlotPhase::Idle);
        assert!(slot.queue.is_empty());
    }

    assert!(
        Arc::ptr_eq(&slot_arc, &ctrl.get_or_create_slot("my-slot")),
        "the same slot name must return the same allocation"
    );
    assert!(
        !Arc::ptr_eq(&slot_arc, &ctrl.get_or_create_slot("other-slot")),
        "different slot names must not share state"
    );
}
