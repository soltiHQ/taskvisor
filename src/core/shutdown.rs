//! Waits for the operating-system signal used by explicit signal mode.
//!
//! [`Supervisor::run_with_os_signals`](crate::Supervisor::run_with_os_signals)
//! calls this helper. Other run modes install no process signal listeners. The
//! helper only waits; the runtime lifecycle code starts shutdown after it
//! returns.
//!
//! Unix waits for `SIGINT`, `SIGTERM`, or `SIGQUIT`. Other platforms use
//! Tokio's Ctrl-C listener. On Unix, installing Tokio signal handlers changes
//! process-global behavior that is not restored when the listeners are dropped.

/// Waits for a supported process shutdown signal.
///
/// The result does not identify which signal arrived.
///
/// # Errors
///
/// Returns an I/O error when a signal listener cannot be installed.
#[cfg(unix)]
pub async fn wait_for_shutdown_signal() -> std::io::Result<()> {
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

/// Waits until the process receives a shutdown signal.
///
/// # Errors
///
/// Returns an I/O error when Ctrl-C waiting fails.
#[cfg(not(unix))]
pub async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}
