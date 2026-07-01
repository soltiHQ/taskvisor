//! # OS shutdown signals.
//!
//! Provides [`wait_for_shutdown_signal`], an async helper used by `Supervisor::run` to wait for a process shutdown signal.
//!
//! This module only waits for OS signals.
//! It does not publish runtime events, cancel tasks, or start shutdown by itself.
//! The caller decides how to handle `Ok(())` and `Err(_)`.
//!
//! ## Unix Signals
//!
//! On Unix platforms, the helper waits for the first of:
//! - `SIGINT`
//! - `SIGTERM`
//! - `SIGQUIT`
//!
//! ## Non-Unix Signals
//!
//! On non-Unix platforms, the helper waits for `tokio::signal::ctrl_c`.
//!
//! ## Errors
//!
//! `Err` means signal listener setup or waiting failed.
//! It must not be treated as a normal shutdown signal.

/// Waits until the process receives a shutdown signal.
///
/// Returns:
/// - `Ok(())` when a supported signal is received,
/// - `Err(e)` when signal listener setup or waiting fails.
///
/// The returned value does not include which signal was received.
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
