//! Contains panics from internal asynchronous operations.
//!
//! [`guarded`] catches panics while polling or destroying a future and returns readable panic text.
//! Registry, controller, and shutdown loops use this boundary to report one failed work unit and continue their own recovery.
//! The boundary does not roll back state.
//! User task attempts use the separate boundary in the attempt runner.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A separately pinned future whose destructor is kept inside the panic boundary.
struct Guarded<F: Future> {
    future: Option<Pin<Box<F>>>,
}

impl<F: Future> Guarded<F> {
    /// Pins a future inside its panic boundary.
    fn new(future: F) -> Self {
        Self {
            future: Some(Box::pin(future)),
        }
    }

    /// Destroys one future without allowing its panic payload (or a nested payload-destructor panic) to unwind into the owner task.
    fn dispose_future(future: Pin<Box<F>>) -> Option<String> {
        match std::panic::catch_unwind(AssertUnwindSafe(|| drop(future))) {
            Ok(()) => None,
            Err(payload) => {
                let message = panic_message(payload.as_ref());
                dispose_panic_payload(payload);
                Some(format!("future cleanup panicked: {message}"))
            }
        }
    }

    /// Destroys an output value without letting its panic escape.
    fn dispose_value<T>(value: T) {
        if let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(value))) {
            dispose_panic_payload(payload);
        }
    }
}

impl<F: Future> Future for Guarded<F> {
    type Output = Result<F::Output, String>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(future) = self.future.as_mut() else {
            return Poll::Ready(Err("guarded future polled after completion".to_owned()));
        };
        match std::panic::catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(output)) => {
                let future = self
                    .future
                    .take()
                    .expect("a ready guarded future is destroyed exactly once");
                if let Some(message) = Self::dispose_future(future) {
                    Self::dispose_value(output);
                    Poll::Ready(Err(message))
                } else {
                    Poll::Ready(Ok(output))
                }
            }
            Err(payload) => {
                let message = panic_message(payload.as_ref());
                dispose_panic_payload(payload);
                let future = self
                    .future
                    .take()
                    .expect("a panicked guarded future is destroyed exactly once");
                let _cleanup_panic = Self::dispose_future(future);
                Poll::Ready(Err(message))
            }
        }
    }
}

impl<F: Future> Drop for Guarded<F> {
    fn drop(&mut self) {
        if let Some(future) = self.future.take() {
            let _cleanup_panic = Self::dispose_future(future);
        }
    }
}

/// Panic-contained future with readable polling and cleanup failures.
pub(crate) fn guarded<F: Future>(fut: F) -> impl Future<Output = Result<F::Output, String>> {
    Guarded::new(fut)
}

/// Destroys a panic payload without allowing a nested panic to escape.
fn dispose_panic_payload(payload: Box<dyn std::any::Any + Send>) {
    if let Err(nested) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        std::mem::forget(nested);
    }
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
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    struct NestedDropPayload;

    impl Drop for NestedDropPayload {
        fn drop(&mut self) {
            panic!("nested payload drop panic");
        }
    }

    struct PollAndDropPanic {
        dropped: Arc<AtomicBool>,
    }

    impl Future for PollAndDropPanic {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            panic!("poll panic");
        }
    }

    impl Drop for PollAndDropPanic {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
            std::panic::panic_any(NestedDropPayload);
        }
    }

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

    #[tokio::test]
    async fn poll_and_nested_drop_panics_cannot_escape_the_guard() {
        let dropped = Arc::new(AtomicBool::new(false));
        let result = guarded(PollAndDropPanic {
            dropped: Arc::clone(&dropped),
        })
        .await;

        assert!(
            result.is_err_and(|message| message.contains("poll panic")),
            "the polling panic remains the primary diagnostic"
        );
        assert!(
            dropped.load(Ordering::Acquire),
            "the panicked future must be destroyed inside the guard"
        );
    }
}
