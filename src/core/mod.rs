//! Runtime core: orchestration and lifecycle.
//!
//! This module contains the embedded implementation of the taskvisor runtime.
//!
//! ## Files & responsibilities
//!
//! - **supervisor.rs**: public facade;
//!   owns the runtime (Bus, Registry, SubscriberSet, AliveTracker), wires listeners,
//!   sends `Add`/`Remove` commands via mpsc to Registry, publishes `ShutdownRequested`, drives graceful shutdown.
//! - **registry.rs**: task lifecycle manager with two input channels:
//!   - mpsc for commands (`Add`/`Remove`: guaranteed delivery),
//!   - broadcast bus for lifecycle events (`ActorExhausted`/`ActorDead`: cleanup).
//!
//!   Publishes `TaskAdded`/`TaskRemoved` as confirmations.
//! - **actor.rs**: per-task supervision loop (sequential attempts):
//!   - applies Restart/Backoff/Timeout
//!   - calls `runner::run_once`
//!   - publishes TaskStarting/BackoffScheduled and terminal ActorExhausted/ActorDead.
//! - **runner.rs**: executes ONE attempt with optional timeout and child token;
//!   - publishes TaskStopped / TaskFailed / TimeoutHit for observability.
//! - **alive.rs**: sequence-aware "alive" state tracker.
//! - **shutdown.rs**: cross-platform OS signal handling used by `Supervisor`.
//!
//! ## Event data-plane (who publishes & who consumes)
//!
//! Producers (publish to Bus):
//! - **Supervisor** → `ShutdownRequested`, `TaskAddRequested`, `TaskRemoveRequested`
//! - **Registry**   → `TaskAdded{name}`, `TaskRemoved{name}`
//! - **TaskActor**  → `TaskStarting{attempt}`, `BackoffScheduled{delay}`, `ActorExhausted`, `ActorDead`
//! - **Runner**     → `TaskStopped` (success/cancel), `TaskFailed` (fail/fatal/timeout), `TimeoutHit`
//! - **SubscriberSet (workers)** → `SubscriberOverflow`, `SubscriberPanicked`
//!
//! Consumers (subscribe to Bus):
//! - **Supervisor::subscriber_listener()** (single fan-out point)
//!     - updates **AliveTracker** (sequence-based ordering)
//!     - emits to **SubscriberSet** (per-subscriber mpsc queues)
//! - **Registry** (cmd_rx + bus_rx): commands via mpsc, lifecycle cleanup via bus
//!
//! ## Wiring (module-level flow)
//!
//! ```text
//! Application code
//!   └─ builds TaskSpec, creates Supervisor, calls Supervisor::run(specs)
//!
//! Supervisor::run()
//!   ├─ spawn subscriber_listener()   ──┐
//!   ├─ Registry::spawn_listener()      │ both subscribe to Bus
//!   ├─ cmd_tx.send(Add(spec))          │ commands via mpsc (guaranteed)
//!   └─ wait: shutdown signal OR registry empty
//!
//!   Supervisor ──cmd_tx──► Registry (mpsc, guaranteed delivery)
//!                            ├─► Add(spec)    → spawn → publish TaskAdded
//!                            └─► Remove(name) → cancel → publish TaskRemoved
//!
//!                         ┌──────────────────────────── Bus (broadcast) ───────────────────────┐
//! Publishers:             │                                                                    │
//!   Supervisor ─────────► │ ShutdownRequested                                                  │
//!   Registry   ─────────► │ TaskAdded / TaskRemoved                                            │
//!   TaskActor  ─────────► │ TaskStarting / BackoffScheduled / ActorExhausted / ActorDead       │
//!   Runner     ─────────► │ TaskStopped / TaskFailed / TimeoutHit                              │
//!   SubscriberSet ──────► │ SubscriberOverflow / SubscriberPanicked                            │
//!                         └──┬──────────────────────────────────────────┬──────────────────────┘
//!              Supervisor::subscriber_listener()         Registry::spawn_listener()
//!                ├─ AliveTracker::update(ev)               └─ on ActorExhausted/ActorDead → cleanup
//!                └─ SubscriberSet::emit_arc(ev)
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
//!                          - delay = backoff.next(attempt); publish BackoffScheduled{delay}; sleep
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
//!
//! ```text
//! OS signal → Supervisor publishes ShutdownRequested → cancel runtime_token
//! → Registry.cancel_all(): cancel each task, await join, publish TaskRemoved
//! → Supervisor.wait_all_with_grace(): AllStoppedWithinGrace OR GraceExceeded{grace, stuck}
//! ```
//!
//! ## Notes
//!
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

mod config;
pub use config::SupervisorConfig;

mod handle;
pub use handle::SupervisorHandle;

mod supervisor;
pub use supervisor::Supervisor;

mod actor;
mod alive;
mod builder;
mod registry;
mod runner;
mod shutdown;
