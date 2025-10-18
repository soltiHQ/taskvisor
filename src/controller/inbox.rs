use tokio::sync::{mpsc, Mutex, OnceCell};

use crate::controller::spec::ControllerSpec;

/// Global inbox for controller submissions.
static TX: OnceCell<mpsc::Sender<ControllerSpec>> = OnceCell::const_new();
static RX: OnceCell<Mutex<mpsc::Receiver<ControllerSpec>>> = OnceCell::const_new();

/// Initialize the controller inbox (idempotent).
pub fn init(capacity: usize) {
    if TX.get().is_none() {
        let (tx, rx) = mpsc::channel::<ControllerSpec>(capacity);
        let _ = TX.set(tx);
        let _ = RX.set(Mutex::new(rx));
    }
}

/// Submit a request into the controller.
pub async fn submit(spec: ControllerSpec) -> Result<(), mpsc::error::SendError<ControllerSpec>> {
    let tx = TX.get().expect("controller inbox not initialized");
    tx.send(spec).await
}

/// Take a guard over the receiver (for the worker loop).
pub async fn recv_guard()
    -> tokio::sync::MutexGuard<'static, mpsc::Receiver<ControllerSpec>>
{
    RX.get().expect("controller inbox not initialized").lock().await
}