//! Waits for the operating-system signal used by explicit signal mode.
//!
//! [`Supervisor::run_with_os_signals`](crate::Supervisor::run_with_os_signals) calls this helper.
//! Other run modes install no process signal listeners.
//! Runtime lifecycle code starts shutdown after this helper returns.
//!
//! Unix waits for `SIGINT`, `SIGTERM`, or `SIGQUIT`.
//! Other platforms use Tokio's Ctrl-C listener.
//! On Unix, installing Tokio signal handlers changes process-global behavior that is not restored when the listeners are dropped.

/// Next supported process shutdown signal.
///
/// The result does not identify which signal arrived.
///
/// # Errors
///
/// - [`std::io::Error`] when a signal listener cannot be installed.
#[cfg(unix)]
pub(super) async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigquit = signal(SignalKind::quit())?;

    tokio::select! {
        _ = sigint.recv()  => {},
        _ = sigterm.recv() => {},
        _ = sigquit.recv() => {},
    }
    Ok(())
}

/// Next supported process shutdown signal.
///
/// # Errors
///
/// - [`std::io::Error`] when waiting for Ctrl-C fails.
#[cfg(not(unix))]
pub(super) async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}
