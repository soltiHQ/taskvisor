//! # Panic boundary for core listener work.
//!
//! This module provides [`guarded`], a small helper for running one async unit of listener work behind `catch_unwind`.
//!
//! It is used by long-lived runtime listeners, such as:
//! - the registry listener,
//! - the subscriber fan-out listener.
//!
//! Without this boundary, a panic while processing one command or event could kill the spawned listener task.
//! That would leave the runtime alive but unable to process later cleanup events.
//!
//! ## What This Guard Does
//!
//! - Catches panics raised while polling the future.
//! - Converts the panic payload into a string.
//! - Returns `Err(message)` to the caller.
//!
//! ## What This Guard Does Not Do
//!
//! - It does not repair partially updated shared state.
//! - It does not restart a loop by itself.
//! - It does not replace task-body panic handling. Task execution uses the
//!   runner's own panic boundary.
//!
//! The caller decides what to do with `Err(message)`, usually by publishing a diagnostic event and continuing the listener loop.

use std::future::{Future, poll_fn};
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::task::Poll;

/// Runs `fut` to completion under a panic boundary.
///
/// Returns:
/// - `Ok(output)` when the future completes normally,
/// - `Err(message)` when polling the future panics.
///
/// The future is boxed so it can be safely polled through `Pin<&mut F>` without unsafe pin projection.
pub(crate) async fn guarded<F: Future>(fut: F) -> Result<F::Output, String> {
    let mut fut: Pin<Box<F>> = Box::pin(fut);
    poll_fn(
        move |cx| match std::panic::catch_unwind(AssertUnwindSafe(|| fut.as_mut().poll(cx))) {
            Ok(Poll::Ready(out)) => Poll::Ready(Ok(out)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => Poll::Ready(Err(panic_message(&*payload))),
        },
    )
    .await
}

/// Converts a panic payload into a readable message.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&'static str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn normal_outputs_pass_through_before_and_after_await() {
        assert_eq!(guarded(async { 42 }).await, Ok(42));
        assert_eq!(
            guarded(async {
                tokio::task::yield_now().await;
                "done"
            })
            .await,
            Ok("done")
        );
    }

    #[tokio::test]
    async fn panics_before_and_after_await_become_errors() {
        let before: Result<(), String> = guarded(async { panic!("boom") }).await;
        assert!(
            before
                .as_ref()
                .is_err_and(|message| message.contains("boom")),
            "panic before the first await must become Err, got {before:?}"
        );

        let after: Result<(), String> = guarded(async {
            tokio::task::yield_now().await;
            panic!("late {}", 7);
        })
        .await;
        assert!(
            after
                .as_ref()
                .is_err_and(|message| message.contains("late 7")),
            "panic after an await must become Err, got {after:?}"
        );
    }
}
