//! # LogWriter human-readable event printer.
//!
//! A minimal subscriber that prints incoming [`Event`]s to stdout.
//! Useful for development, debugging, and demos.
//!
//! ## Example output
//! ```text
//! [001] [starting] task=worker attempt=1
//! [002] [failed] task=worker reason="connection refused" attempt=1
//! [003] [backoff] task=worker source=failure delay=2s after_attempt=1 reason="connection refused"
//! [004] [timeout] task=worker timeout=5s
//! [005] [stopped] task=worker
//! [006] [actor-exhausted] task=worker reason=policy
//! [007] [task-add-requested] task=new-worker
//! [008] [task-added] task=new-worker
//! [009] [task-remove-requested] task=old-worker
//! [010] [task-removed] task=old-worker
//! [011] [shutdown-requested]
//! [012] [all-stopped-within-grace]
//! ```
//!
//! ## Example
//! ```rust,ignore
//! use taskvisor::{Supervisor, Config, Subscribe};
//! use taskvisor::subscribers::log_writer::LogWriter;
//! use std::sync::Arc;
//!
//! let subs: Vec<Arc<dyn Subscribe>> = vec![Arc::new(LogWriter::default())];
//! let sup = Supervisor::new(Config::default(), subs);
//! ```

use crate::events::{BackoffSource, Event, EventKind};
use crate::subscribers::Subscribe;
use async_trait::async_trait;

/// Human-readable event printer for stdout.
///
/// Prints `[seq] [event-type] key=value ...` with relevant metadata.
/// Useful for debugging, demos, or understanding supervisor flow.
#[derive(Default)]
pub struct LogWriter;

#[async_trait]
impl Subscribe for LogWriter {
    async fn on_event(&self, e: &Event) {
        let seq = format!("[{:03}]", e.seq % 1000);

        fn fmt_ms(ms: Option<u32>) -> String {
            match ms {
                Some(v) if v >= 1000 && v % 1000 == 0 => format!("{}s", v / 1000),
                Some(v) if v >= 1000 => format!("{:.1}s", v as f64 / 1000.0),
                Some(v) => format!("{}ms", v),
                None => "0ms".to_string(),
            }
        }
        fn or<'a>(s: Option<&'a str>, def: &'a str) -> &'a str {
            s.unwrap_or(def)
        }

        match e.kind {
            // === Shutdown ===
            EventKind::ShutdownRequested => {
                println!("{} [shutdown-requested]", seq);
            }
            EventKind::AllStoppedWithin => {
                println!("{} [all-stopped-within-grace]", seq);
            }
            EventKind::GraceExceeded => {
                println!("{} [grace-exceeded]", seq);
            }

            // === Task lifecycle ===
            EventKind::TaskStarting => {
                println!(
                    "{} [starting] task={} attempt={}",
                    seq,
                    or(e.task.as_deref(), "none"),
                    e.attempt.unwrap_or(0)
                );
            }
            EventKind::TaskStopped => {
                println!("{} [stopped] task={}", seq, or(e.task.as_deref(), "none"));
            }
            EventKind::TaskFailed => {
                println!(
                    "{} [failed] task={} reason=\"{}\" attempt={}",
                    seq,
                    or(e.task.as_deref(), "none"),
                    or(e.reason.as_deref(), "unknown"),
                    e.attempt.unwrap_or(0)
                );
            }
            EventKind::TimeoutHit => {
                println!(
                    "{} [timeout] task={} timeout={}",
                    seq,
                    or(e.task.as_deref(), "none"),
                    fmt_ms(e.timeout_ms)
                );
            }
            EventKind::BackoffScheduled => {
                let src = match e.backoff_source {
                    Some(BackoffSource::Success) => "success",
                    Some(BackoffSource::Failure) => "failure",
                    None => "unknown",
                };
                println!(
                    "{} [backoff] task={} source={} delay={} after_attempt={} reason=\"{}\"",
                    seq,
                    or(e.task.as_deref(), "none"),
                    src,
                    fmt_ms(e.delay_ms),
                    e.attempt.unwrap_or(0),
                    or(e.reason.as_deref(), "none")
                );
            }

            // === Subscribers ===
            EventKind::SubscriberOverflow => {
                println!(
                    "{} [subscriber-overflow] subscriber={} reason=\"{}\"",
                    seq,
                    or(e.task.as_deref(), "none"),
                    or(e.reason.as_deref(), "unknown")
                );
            }
            EventKind::SubscriberPanicked => {
                println!(
                    "{} [subscriber-panicked] subscriber={} info=\"{}\"",
                    seq,
                    or(e.task.as_deref(), "none"),
                    or(e.reason.as_deref(), "unknown")
                );
            }

            // === Management ===
            EventKind::TaskAddRequested => {
                println!(
                    "{} [task-add-requested] task={}",
                    seq,
                    or(e.task.as_deref(), "none")
                );
            }
            EventKind::TaskAdded => {
                println!(
                    "{} [task-added] task={}",
                    seq,
                    or(e.task.as_deref(), "none")
                );
            }
            EventKind::TaskRemoveRequested => {
                println!(
                    "{} [task-remove-requested] task={}",
                    seq,
                    or(e.task.as_deref(), "none")
                );
            }
            EventKind::TaskRemoved => {
                println!(
                    "{} [task-removed] task={}",
                    seq,
                    or(e.task.as_deref(), "none")
                );
            }

            // === Terminals ===
            EventKind::ActorExhausted => {
                println!(
                    "{} [actor-exhausted] task={} reason=\"{}\"",
                    seq,
                    or(e.task.as_deref(), "none"),
                    or(e.reason.as_deref(), "policy")
                );
            }
            EventKind::ActorDead => {
                println!(
                    "{} [actor-dead] task={} reason=\"{}\"",
                    seq,
                    or(e.task.as_deref(), "none"),
                    or(e.reason.as_deref(), "fatal")
                );
            }
        }
    }

    fn name(&self) -> &'static str {
        "LogWriter"
    }
}
