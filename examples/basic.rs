//! # Basic: run one task
//!
//! This example shows the smallest static setup: create a task, run it once,
//! and wait for the supervisor to finish.
//!
//! It uses:
//!
//! - `TaskFn::arc` to turn an async closure into a `TaskRef`;
//! - `TaskSpec::once` to disable restarts;
//! - `Supervisor::run` to start the task and wait for completion.
//!
//! The task finishes at once, so it does not use its `TaskContext`. A
//! long-running task should observe cancellation; see `worker.rs`.
//!
//! Run with `cargo run --example basic`.

use taskvisor::prelude::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let task: TaskRef = TaskFn::arc("hello", |_ctx| async move {
        println!("Hello from taskvisor!");
        Ok(())
    });

    let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    sup.run(vec![TaskSpec::once(task)]).await?;

    println!("Done.");
    Ok(())
}
