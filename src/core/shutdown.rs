//! # OS shutdown signal helper
//!
//! [`Supervisor::run_with_os_signals`](crate::Supervisor::run_with_os_signals) uses [`wait_for_shutdown_signal`] as an explicit opt-in.
//! [`Supervisor::run`](crate::Supervisor::run), [`Supervisor::run_until`](crate::Supervisor::run_until), and dynamic mode through [`Supervisor::serve`](crate::Supervisor::serve) do not install signal listeners.
//!
//! This helper installs signal listeners and waits.
//! It does not publish events or cancel tasks.
//!
//! On Unix, Tokio's process-global signal handling is not restored when these listeners are dropped.
//! Callers remain responsible for process signals after opting in.
//!
//! ## Unix Signals
//!
//! On Unix, it waits for the first of:
//! - `SIGINT`
//! - `SIGTERM`
//! - `SIGQUIT`
//!
//! ## Non-Unix Signals
//!
//! On other platforms, it waits for `tokio::signal::ctrl_c`.
//!
//! ## Errors
//!
//! An error means signal setup failed or, on a non-Unix platform, waiting failed.
//! It is not a shutdown signal.

/// Waits for a supported process shutdown signal.
///
/// Returns:
/// - `Ok(())` when a supported signal is received,
/// - `Err(e)` when a signal listener cannot be installed.
///
/// The result does not identify which signal arrived.
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
/// Returns:
/// - `Ok(())` when Ctrl-C is received,
/// - `Err(e)` when signal waiting fails.
#[cfg(not(unix))]
pub async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}
