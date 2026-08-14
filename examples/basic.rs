//! # Basic: run one task
//!
//! Use this pattern when all tasks are known at startup and finish on their own.
//!
//! `TaskFn::arc` adapts an async closure into a shared task.
//! `TaskSpec::once` permits one attempt.
//! `Supervisor::run` admits the initial batch and returns after registry membership is empty and bounded cleanup finishes.
//!
//! `run` reports the supervisor lifecycle.
//! Its `Ok(())` does not prove that every task succeeded.
//! Use a watched task when application logic needs its final outcome; see `outcomes.rs`.
//!
//! This task prints once and completes immediately. It does not use `TaskContext`.
//! See `graceful_worker.rs` for a long-running task that observes cancellation.
//!
//! Run with `cargo run --example basic`.

use taskvisor::prelude::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let task: TaskRef = TaskFn::arc(|_ctx| async move {
        println!("Hello from taskvisor!");
        Ok(())
    });

    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    supervisor.run(vec![TaskSpec::once("hello", task)]).await?;

    Ok(())
}
