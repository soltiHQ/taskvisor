use std::sync::Arc;
use tokio::sync::OnceCell;
use crate::Supervisor;

/// Global access to Supervisor from the controller loop.
static SUP: OnceCell<Arc<Supervisor>> = OnceCell::const_new();

/// Bind a Supervisor instance for the controller (call once before run()).
pub fn bind_supervisor(sup: Arc<Supervisor>) {
    let _ = SUP.set(sup);
}

/// Get a bound Supervisor (panics if not bound).
pub fn supervisor() -> Arc<Supervisor> {
    SUP.get().expect("controller supervisor not bound").clone()
}