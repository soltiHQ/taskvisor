//! Measures the held-task allocation footprint.

mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::future::Future;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;

use taskvisor::{BoxTaskFuture, Supervisor, Task, TaskContext, TaskError, TaskRef, TaskSpec};

use support::fixtures::{
    AsyncCounter, OWNERSHIP_CAPACITY, bench_config, expect_within, rt_current_thread,
    wait_for_ownership, warm_runtime,
};

const CHILD_COUNT_ENV: &str = "TASKVISOR_MEMORY_BENCH_CHILD_COUNT";
const POPULATIONS: [usize; 4] = [1, 32, 256, OWNERSHIP_CAPACITY];

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE_BLOCKS: AtomicUsize = AtomicUsize::new(0);

struct TrackingAllocator;

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
            LIVE_BLOCKS.fetch_add(1, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
            LIVE_BLOCKS.fetch_add(1, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        LIVE_BLOCKS.fetch_sub(1, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            if new_size >= layout.size() {
                LIVE_BYTES.fetch_add(new_size - layout.size(), Ordering::Relaxed);
            } else {
                LIVE_BYTES.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        new_ptr
    }
}

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

struct PendingTask {
    started: Arc<AsyncCounter>,
}

impl Task for PendingTask {
    fn spawn(&self, ctx: TaskContext) -> BoxTaskFuture {
        let started = Arc::clone(&self.started);
        Box::pin(async move {
            let cancelled = ctx.cancelled();
            tokio::pin!(cancelled);
            let mut announced = false;
            std::future::poll_fn(|cx| match cancelled.as_mut().poll(cx) {
                Poll::Ready(()) => Poll::Ready(()),
                Poll::Pending => {
                    if !announced {
                        announced = true;
                        started.increment();
                    }
                    Poll::Pending
                }
            })
            .await;
            Err(TaskError::Canceled)
        })
    }
}

#[derive(Clone, Copy)]
struct Sample {
    tasks: usize,
    bytes: usize,
    blocks: usize,
}

fn live_bytes() -> usize {
    LIVE_BYTES.load(Ordering::Relaxed)
}

fn live_blocks() -> usize {
    LIVE_BLOCKS.load(Ordering::Relaxed)
}

fn measure(tasks: usize) -> Sample {
    assert!((1..=OWNERSHIP_CAPACITY).contains(&tasks));
    let runtime = rt_current_thread();
    runtime.block_on(async move {
        let handle = Supervisor::new(bench_config(), vec![])
            .serve()
            .expect("runtime startup");
        warm_runtime(&handle, 0).await;

        let started = AsyncCounter::new();
        let task: TaskRef = Arc::new(PendingTask {
            started: Arc::clone(&started),
        });
        let names: Vec<Arc<str>> = (0..tasks)
            .map(|index| Arc::<str>::from(format!("memory-held-{index:04}")))
            .collect();
        let mut ids = Vec::with_capacity(tasks);
        tokio::task::yield_now().await;

        let baseline_bytes = live_bytes();
        let baseline_blocks = live_blocks();

        for name in &names {
            let id = expect_within(
                "memory sample admission",
                handle
                    .add(TaskSpec::once(Arc::clone(name), Arc::clone(&task)))
                    .execute(),
            )
            .await
            .expect("memory sample admission failed");
            ids.push(id);
        }
        started.wait_for(tasks).await;
        wait_for_ownership(&handle, tasks).await;
        tokio::task::yield_now().await;

        let bytes = live_bytes()
            .checked_sub(baseline_bytes)
            .expect("live heap bytes fell below the warmed baseline");
        let blocks = live_blocks()
            .checked_sub(baseline_blocks)
            .expect("live heap blocks fell below the warmed baseline");

        for id in ids {
            assert!(
                expect_within("memory sample cancellation", handle.cancel(id).execute())
                    .await
                    .expect("memory sample cancellation failed"),
                "memory sample must claim every held task",
            );
        }
        wait_for_ownership(&handle, 0).await;
        handle.shutdown().await.expect("runtime shutdown failed");

        Sample {
            tasks,
            bytes,
            blocks,
        }
    })
}

fn child_main(tasks: usize) {
    let sample = measure(tasks);
    println!("{} {} {}", sample.tasks, sample.bytes, sample.blocks);
}

fn run_isolated_sample(executable: &std::path::Path, tasks: usize) -> Sample {
    let output = Command::new(executable)
        .env(CHILD_COUNT_ENV, tasks.to_string())
        .output()
        .expect("memory sample child process failed to start");
    assert!(
        output.status.success(),
        "memory sample child failed for {tasks} tasks: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).expect("memory sample output was not UTF-8");
    let mut fields = stdout.split_whitespace();
    let parsed_tasks: usize = fields
        .next()
        .expect("memory sample omitted task count")
        .parse()
        .expect("memory sample task count was not numeric");
    let bytes = fields
        .next()
        .expect("memory sample omitted live bytes")
        .parse()
        .expect("memory sample live bytes were not numeric");
    let blocks = fields
        .next()
        .expect("memory sample omitted live blocks")
        .parse()
        .expect("memory sample live blocks were not numeric");
    assert_eq!(parsed_tasks, tasks, "memory sample task count changed");
    assert!(
        fields.next().is_none(),
        "memory sample emitted extra fields"
    );
    Sample {
        tasks,
        bytes,
        blocks,
    }
}

fn print_report(samples: &[Sample]) {
    println!();
    println!("TASKVISOR MEMORY FOOTPRINT · MINIMAL HELD TASKS");
    let build = support::git_revision().map_or_else(
        || format!("taskvisor {}", env!("CARGO_PKG_VERSION")),
        |revision| format!("taskvisor {} · {revision}", env!("CARGO_PKG_VERSION")),
    );
    println!("Build: {build}");
    println!(
        "Platform: {} · {}",
        support::display_os(env::consts::OS),
        env::consts::ARCH,
    );
    println!("Features: {}", support::enabled_features());
    println!("Runtime: Tokio current-thread · no subscribers");
    println!("Measurement: live requested bytes and allocation count through std::alloc::System");
    println!("Isolation: one fresh process per population; no pass/fail threshold");
    println!();
    println!(
        "held tasks | live bytes delta | bytes/task | live allocations delta | allocations/task"
    );
    println!(
        "-----------+------------------+------------+------------------------+-----------------"
    );
    for sample in samples {
        println!(
            "{:>10} | {:>16} | {:>10.2} | {:>22} | {:>16.2}",
            sample.tasks,
            sample.bytes,
            sample.bytes as f64 / sample.tasks as f64,
            sample.blocks,
            sample.blocks as f64 / sample.tasks as f64,
        );
    }
    println!();
    println!(
        "Boundary: warmed supervisor to minimal unwatched tasks whose bodies were observed at Poll::Pending"
    );
    println!(
        "Excluded: prebuilt unique names, ID-vector storage, shared task object, startup, cleanup, allocator metadata, direct native allocations, and RSS"
    );
    println!(
        "Interpretation: fixture-specific retained allocation footprint on this build, not a universal bytes-per-task constant"
    );
}

fn main() {
    if let Some(tasks) = env::var_os(CHILD_COUNT_ENV) {
        let tasks = tasks
            .to_string_lossy()
            .parse()
            .expect("memory child task count was not numeric");
        child_main(tasks);
        return;
    }

    let executable = env::current_exe().expect("memory benchmark executable path");
    let samples: Vec<_> = POPULATIONS
        .into_iter()
        .map(|tasks| run_isolated_sample(&executable, tasks))
        .collect();
    print_report(&samples);
}
