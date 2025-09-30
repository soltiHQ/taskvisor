//! # Cross-platform OS signal handling.
//!
//! This module provides a single async helper [`wait_for_shutdown_signal`]
//! that completes when the process receives a termination signal.
//!
//! ## Unix
//! On Unix platforms the following signals are handled:
//! - **SIGINT** (Ctrl-C in terminal)
//! - **SIGTERM** (default kill signal, used by systemd/Kubernetes)
//! - **SIGQUIT** (optional "quit" signal, often used for core dumps or hard stop)
//!
//! Additionally, [`tokio::signal::ctrl_c`] is awaited as a fallback.
//!
//! ## Windows
//! On non-Unix platforms only [`tokio::signal::ctrl_c`] is awaited.

/// Waits until a shutdown signal is received and then returns `Ok(())`.
///
/// # Errors
/// Returns `Err` only if OS signal registration/awaiting fails.
///
/// # Notes
/// - Multiple concurrent callers are allowed; all will resolve on the same OS signal.
/// - On Unix, SIGINT/SIGTERM/SIGQUIT и `ctrl_c()`.
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

#[cfg(not(unix))]
pub async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}
