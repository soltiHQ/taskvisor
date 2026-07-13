//! # Panic boundary for runtime listeners
//!
//! [`guarded`] catches a panic while one listener item is processed.
//! This stops a bad command or event from silently killing a long-running registry or subscriber listener.
//!
//! ## What This Guard Does
//!
//! It catches panics while polling the future and returns the panic text as `Err(message)`.
//!
//! ## What This Guard Does Not Do
//!
//! It does not repair partially changed state or restart a loop.
//! User task bodies use a separate panic boundary in the attempt runner.
//!
//! The caller normally publishes a diagnostic event and continues the loop.

use std::future::{Future, poll_fn};
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::task::Poll;

/// Runs `fut` behind a panic boundary.
///
/// Returns:
/// - `Ok(output)` when the future completes normally,
/// - `Err(message)` when polling the future panics.
///
/// Boxing keeps pinning safe without custom unsafe projection.
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
