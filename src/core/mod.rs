//! Runtime core: orchestration and lifecycle.
//!
//! This module contains the embedded implementation of the taskvisor runtime.
//! The only public API re-exported from here is [`Supervisor`]. Everything else
//! is an internal building block that the supervisor wires together.
//!
//! ## Files & responsibilities
//! - **supervisor.rs**: public facade; owns the runtime (Bus, Registry, SubscriberSet, AliveTracker),
//!   wires listeners, publishes control events (ShutdownRequested, TaskAddRequested, TaskRemoveRequested),
//!   drives graceful shutdown.
//! - **registry.rs**: event-driven task lifecycle: listens to Bus; on TaskAddRequested spawns a
//!   `TaskActor`; on TaskRemoveRequested cancels & joins; on ActorExhausted/ActorDead cleans up;
//!   publishes TaskAdded/TaskRemoved (and TaskFailed on internal errors like duplicates/lag).
//! - **actor.rs**: per-task supervision loop (sequential attempts): applies Restart/Backoff/Timeout,
//!   calls `runner::run_once`, publishes TaskStarting/BackoffScheduled and terminal ActorExhausted/ActorDead.
//! - **runner.rs**: executes ONE attempt with optional timeout and child token; publishes
//!   TaskStopped / TaskFailed / TimeoutHit for observability.
//! - **alive.rs**: sequence-aware “alive” state tracker (TaskStarting → alive=true; TaskStopped/TaskFailed → false).
//! - **shutdown.rs**: cross-platform OS signal handling used by `Supervisor`.
//!
//! ## Event data-plane (who publishes & who consumes)
//!
//! Producers (publish to Bus):
//! - **Supervisor** → `ShutdownRequested`, `TaskAddRequested{spec}`, `TaskRemoveRequested{name}`
//! - **Registry**   → `TaskAdded{name}`, `TaskRemoved{name}`, `TaskFailed{internal errors}`
//! - **TaskActor**  → `TaskStarting{attempt}`, `BackoffScheduled{delay}`, `ActorExhausted`, `ActorDead`
//! - **Runner**     → `TaskStopped` (success/cancel), `TaskFailed` (fail/fatal/timeout), `TimeoutHit`
//! - **SubscriberSet (workers)** → `SubscriberOverflow`, `SubscriberPanicked`
//!
//! Consumers (subscribe to Bus):
//! - **Supervisor::subscriber_listener()** (single fan-out point)
//!     - updates **AliveTracker** (sequence-based ordering)
//!     - emits to **SubscriberSet** (per-subscriber mpsc queues)
//! - **Registry** (its own listener): reacts to management/terminal events listed above
//!
//! ## Wiring (module-level flow)
//! ```text
//! Application code
//!   └─ builds TaskSpec, creates Supervisor, calls Supervisor::run(specs)
//!
//! Supervisor::run()
//!   ├─ spawn subscriber_listener()   ──┐
//!   ├─ Registry::spawn_listener()      │ both subscribe to Bus
//!   ├─ publish TaskAddRequested{spec}  │
//!   └─ wait: shutdown signal OR registry empty
//!
//!                         ┌──────────────────────────── Bus (broadcast) ───────────────────────┐
//! Publishers:             │                                                                    │
//!   Supervisor ─────────► │ TaskAddRequested / TaskRemoveRequested / ShutdownRequested         │
//!   Registry   ─────────► │ TaskAdded / TaskRemoved / TaskFailed(internal)                     │
//!   TaskActor  ─────────► │ TaskStarting / BackoffScheduled / ActorExhausted / ActorDead       │
//!   Runner     ─────────► │ TaskStopped / TaskFailed / TimeoutHit                              │
//!   SubscriberSet ──────► │ SubscriberOverflow / SubscriberPanicked                            │
//!                         └──┬──────────────────────────────────────────┬──────────────────────┘
//!              Supervisor::subscriber_listener()         Registry::spawn_listener()
//!                ├─ AliveTracker::update(ev)               ├─ on TaskAddRequested → spawn TaskActor
//!                └─ SubscriberSet::emit{,_arc}(ev)         ├─ on TaskRemoveRequested → cancel+join
//!                                                          └─ on ActorExhausted/ActorDead → cleanup
//!
//! TaskActor::run()  (per task)
//! loop {
//!   acquire global semaphore (optional, cancellable)
//!   publish TaskStarting{attempt}
//!   result = runner::run_once(task, timeout, attempt, bus)
//!   match result {
//!     Ok                → if RestartPolicy::Always continue; else publish ActorExhausted & exit
//!     Err(Fatal)        → publish ActorDead & exit
//!     Err(Canceled)     → exit (cooperative shutdown)
//!     Err(Timeout/Fail) → if policy allows retry:
//!                          - delay = backoff.next(prev); publish BackoffScheduled{delay}; sleep
//!                          - else publish ActorExhausted & exit
//!   }
//! }
//!
//! runner::run_once()
//!   - derive child token
//!   - if timeout set → time::timeout(fut); on elapsed: cancel child, publish TimeoutHit, Timeout
//!   - on Ok or Err(Canceled) → publish TaskStopped
//!   - on Err(Fail/Fatal/Timeout) → publish TaskFailed
//! ```
//!
//! ## Shutdown timeline
//! ```text
//! OS signal → Supervisor publishes ShutdownRequested → cancel runtime_token
//! → Registry.cancel_all(): cancel each task, await join, publish TaskRemoved
//! → Supervisor.wait_all_with_grace(): AllStoppedWithinGrace OR GraceExceeded{grace, stuck}
//! ```
//!
//! ## Notes
//! - Event ordering is maintained via a global monotonic sequence number.
//! - Event delivery is fire-and-forget (bounded broadcast + per-subscriber mpsc).
//! - Attempts within a TaskActor are strictly sequential (never parallel for the same task).
//!
//! Internal modules:
//! - [`runner`]     one attempt with timeout/cancellation & lifecycle events
//! - [`supervisor`] orchestrator; owns Bus/Registry/Alive/Subscribers; shutdown
//! - [`actor`]      single-task supervisor with restart/backoff/jitter/timeouts
//! - [`shutdown`]   OS signal handling
//! - [`registry`]   task lifecycle: spawn/cancel/join/cleanup
//! - [`alive`]      sequence-based alive tracking

mod actor;
mod alive;
mod registry;
mod runner;
mod shutdown;
mod supervisor;

pub use supervisor::Supervisor;
