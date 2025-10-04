//! # Task Patterns Example
//!
//! Demonstrates the correct way to build task logic on top of taskvisor.
//!
//! This example shows:
//! - How to separate business logic from task orchestration
//! - Proper patterns for periodic tasks (with loops)
//! - Proper patterns for one-shot tasks (RestartAlways)
//! - Correct cancellation handling
//! - How to configure TaskSpec with different policies

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use taskvisor::{
    BackoffPolicy, Config, JitterPolicy, LogWriter, RestartPolicy, Subscribe, Supervisor,
    TaskError, TaskFn, TaskRef, TaskSpec,
};

/// ================================================================================
/// BUSINESS LOGIC LAYER
/// ================================================================================
///
/// This is where your actual work happens. These are pure Rust functions/structs
/// that don't know anything about taskvisor. They just do the work.

/// Simulates periodic health check with occasional failures
async fn health_check_worker(ctx: CancellationToken) -> Result<(), TaskError> {
    let mut counter = 0u64;

    loop {
        counter += 1;

        // Simulate work
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Simulate random failures (30% chance)
        if rand::random::<f32>() < 0.3 {
            return Err(TaskError::Fail {
                error: format!("Health check #{counter} failed"),
            });
        }
        // Wait before next check
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(3)) => {},
            _ = ctx.cancelled() => {
                return Err(TaskError::Canceled);
            }
        }
    }
}

/// Simulates a worker that processes one item and exits
async fn process_item_worker(ctx: CancellationToken) -> Result<(), TaskError> {
    if ctx.is_cancelled() {
        return Err(TaskError::Canceled);
    }
    let item_id = rand::random::<u32>() % 100;

    tokio::time::sleep(Duration::from_secs(2)).await;

    if rand::random::<f32>() < 0.2 {
        return Err(TaskError::Fail {
            error: format!("Failed to process item #{item_id}"),
        });
    }
    Ok(())
}

/// ================================================================================
/// TASKVISOR ABSTRACTION LAYER
/// ================================================================================
///
/// This layer wraps business logic into taskvisor tasks with proper policies.
/// This is where you define HOW tasks should behave (restart, backoff, timeout).

/// Helper function to create TaskSpec from business logic
fn create_task<Fnc, Fut>(
    name: &'static str,
    worker: Fnc,
    restart: RestartPolicy,
    backoff: BackoffPolicy,
    timeout: Option<Duration>,
) -> TaskSpec
where
    Fnc: Fn(CancellationToken) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<(), TaskError>> + Send + 'static,
{
    let task: TaskRef = TaskFn::arc(name, worker);
    TaskSpec::new(task, restart, backoff, timeout)
}

/// Creates a periodic health check task that restarts on failure
fn health_check_task() -> TaskSpec {
    create_task(
        "health-check",
        |ctx| async move { health_check_worker(ctx).await },
        RestartPolicy::OnFailure, // Restart only on failure (success = infinite loop)
        BackoffPolicy {
            first: Duration::from_secs(1),
            max: Duration::from_secs(5),
            factor: 2.0,
            jitter: JitterPolicy::Equal, // Prevent thundering herd
        },
        Some(Duration::from_secs(30)), // Timeout per attempt
    )
}

/// Creates a worker task that always restarts (processes items continuously)
fn worker_task() -> TaskSpec {
    create_task(
        "worker",
        |ctx| async move { process_item_worker(ctx).await },
        RestartPolicy::Always, // Process next item after success OR retry after failure
        BackoffPolicy {
            first: Duration::from_millis(500),
            max: Duration::from_secs(10),
            factor: 3.0,
            jitter: JitterPolicy::Equal,
        },
        None, // No timeout
    )
}

/// ================================================================================
/// MAIN: SUPERVISOR SETUP
/// ================================================================================
///
/// This is where you configure the runtime and run tasks.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("🚀 Task Patterns Demo");
    println!("   - HealthCheck: periodic task with RestartPolicy::OnFailure");
    println!("   - Worker: one-shot task with RestartPolicy::Always");
    println!("   Press Ctrl+C to trigger graceful shutdown\n");

    // Configure supervisor
    let config = Config {
        grace: Duration::from_secs(5), // Wait 5s for graceful shutdown
        max_concurrent: 0,             // No concurrency limit
        bus_capacity: 1024,            // Event bus capacity
        ..Default::default()
    };

    // Setup subscribers (using built-in LogWriter for demo)
    let subscribers: Vec<std::sync::Arc<dyn Subscribe>> = vec![std::sync::Arc::new(LogWriter)];

    // Create supervisor
    let supervisor = Supervisor::new(config, subscribers);

    // Define tasks
    let tasks = vec![health_check_task(), worker_task()];

    // Run until Ctrl+C or all tasks exit
    match supervisor.run(tasks).await {
        Ok(()) => {
            println!("\n✅ All tasks completed gracefully");
            Ok(())
        }
        Err(e) => {
            println!("\n⚠️  Runtime error: {}", e);
            Err(e.into())
        }
    }
}
