//! # Cross-platform OS signal handling.
//!
//! Provides [`wait_for_shutdown_signal`] an async helper that completes when the process receives a termination signal.
//!
//! ## Signals
//! **Unix platforms:**
//! - `SIGINT` (Ctrl-C in terminal)
//! - `SIGTERM` (default kill signal, used by systemd/Kubernetes)
//! - `SIGQUIT` (quit signal, often used for core dumps or hard stop)
//!
//! **Windows platforms:**
//! - `Ctrl-C` via [`tokio::signal::ctrl_c`]

/// Waits for a termination signal.
///
/// Each call creates independent signal listeners.
///
/// Returns `Ok(())` when any signal is received, or `Err` if signal registration fails.
#[cfg(unix)]
pub async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigquit = signal(SignalKind::quit())?;

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = sigint.recv()  => {},
        _ = sigterm.recv() => {},
        _ = sigquit.recv() => {},
    }
    Ok(())
}

/// Waits for a termination signal.
///
/// Each call creates independent signal listeners.
///
/// Returns `Ok(())` when any signal is received, or `Err` if signal registration fails.
#[cfg(not(unix))]
pub async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}
