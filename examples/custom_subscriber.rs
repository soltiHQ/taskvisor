//! # Custom Subscriber Example
//!
//! Demonstrates how to implement custom subscribers for observability.
//!
//! This example shows:
//! - How to implement the Subscribe trait
//! - Collecting metrics from task events
//! - Proper async processing in subscribers
//! - Single focused task with restart policy

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use taskvisor::{
    BackoffPolicy, Config, Event, EventKind, JitterPolicy, RestartPolicy, Subscribe, Supervisor,
    TaskError, TaskFn, TaskRef, TaskSpec,
};

/// ================================================================================
/// BUSINESS LOGIC LAYER
/// ================================================================================

/// Simulates a worker that occasionally fails to demonstrate restart behavior
async fn worker(ctx: CancellationToken) -> Result<(), TaskError> {
    if ctx.is_cancelled() {
        return Err(TaskError::Canceled);
    }

    let job_id = rand::random::<u32>() % 100;

    // Simulate work
    tokio::time::sleep(Duration::from_millis(2500)).await;

    // Fail 25% of the time
    if rand::random::<f32>() < 0.25 {
        return Err(TaskError::Fail {
            error: format!("Job #{job_id} processing error"),
        });
    }
    Ok(())
}

/// ================================================================================
/// CUSTOM SUBSCRIBER: METRICS COLLECTOR
/// ================================================================================

/// Metrics collector that tracks task execution statistics
struct MetricsCollector {
    total_starts: AtomicU64,
    total_successes: AtomicU64,
    total_failures: AtomicU64,
    total_retries: AtomicU64,
}

impl MetricsCollector {
    fn new() -> Self {
        Self {
            total_starts: AtomicU64::new(0),
            total_successes: AtomicU64::new(0),
            total_failures: AtomicU64::new(0),
            total_retries: AtomicU64::new(0),
        }
    }

    fn print_stats(&self) {
        let starts = self.total_starts.load(Ordering::Relaxed);
        let successes = self.total_successes.load(Ordering::Relaxed);
        let failures = self.total_failures.load(Ordering::Relaxed);
        let retries = self.total_retries.load(Ordering::Relaxed);

        println!("\n📊 [Metrics] Current stats:");
        println!("   • Total starts:    {}", starts);
        println!("   • Total successes: {}", successes);
        println!("   • Total failures:  {}", failures);
        println!("   • Total retries:   {}", retries);

        if starts > 0 {
            let success_rate = (successes as f64 / starts as f64) * 100.0;
            println!("   • Success rate:    {:.1}%\n", success_rate);
        }
    }
}

#[async_trait]
impl Subscribe for MetricsCollector {
    async fn on_event(&self, event: &Event) {
        match event.kind {
            EventKind::TaskStarting => {
                self.total_starts.fetch_add(1, Ordering::Relaxed);
                println!(
                    "📊 [Metrics] Task started (attempt #{})",
                    event.attempt.unwrap_or(0)
                );
            }
            EventKind::TaskStopped => {
                self.total_successes.fetch_add(1, Ordering::Relaxed);
                println!("📊 [Metrics] Task succeeded");
                self.print_stats();
            }
            EventKind::TaskFailed => {
                self.total_failures.fetch_add(1, Ordering::Relaxed);
                println!(
                    "📊 [Metrics] Task failed: {}",
                    event.error.as_deref().unwrap_or("unknown")
                );
            }
            EventKind::BackoffScheduled => {
                self.total_retries.fetch_add(1, Ordering::Relaxed);
                println!(
                    "📊 [Metrics] Retry scheduled (delay: {:?})",
                    event.delay.unwrap_or_default()
                );
            }
            EventKind::ShutdownRequested => {
                println!("📊 [Metrics] Shutdown signal received");
                self.print_stats();
            }
            _ => {}
        }
    }

    fn name(&self) -> &'static str {
        "metrics-collector"
    }

    fn queue_capacity(&self) -> usize {
        512 // Smaller queue for metrics (fast processing)
    }
}

/// ================================================================================
/// TASKVISOR ABSTRACTION LAYER
/// ================================================================================

fn create_worker_task() -> TaskSpec {
    let task: TaskRef = TaskFn::arc("worker", |ctx| async move { worker(ctx).await });

    TaskSpec::new(
        task,
        RestartPolicy::Always, // Keep processing jobs continuously
        BackoffPolicy {
            first: Duration::from_millis(500),
            max: Duration::from_secs(3),
            factor: 1.5,
            jitter: JitterPolicy::Equal,
        },
        Some(Duration::from_secs(5)), // Timeout per job
    )
}

/// ================================================================================
/// MAIN: SUPERVISOR SETUP WITH CUSTOM SUBSCRIBER
/// ================================================================================

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("🚀 Custom Subscriber Demo");
    println!("   Demonstrates metrics collection via custom subscriber");
    println!("   Watch how MetricsCollector tracks task execution\n");
    println!("   Press Ctrl+C for graceful shutdown\n");
    println!("{}\n", "=".repeat(60));

    let config = Config {
        grace: Duration::from_secs(3),
        bus_capacity: 256,
        ..Default::default()
    };

    // Create custom subscriber
    let metrics = Arc::new(MetricsCollector::new());

    // Setup subscribers list
    let subscribers: Vec<Arc<dyn Subscribe>> = vec![
        metrics, // Our custom metrics collector
    ];

    // Create supervisor
    let supervisor = Supervisor::new(config, subscribers);

    // Define tasks
    let tasks = vec![create_worker_task()];

    match supervisor.run(tasks).await {
        Ok(()) => {
            println!("\n✅ Shutdown completed gracefully");
            Ok(())
        }
        Err(e) => {
            println!("\n⚠️  Runtime error: {}", e);
            Err(e.into())
        }
    }
}
