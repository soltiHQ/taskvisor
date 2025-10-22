//! # Controller: slot-based admission & start-on-`TaskRemoved`
//!
//! The **controller** is a thin policy layer (wrapper) over `TaskSpec` submission.
//! It enforces per-slot admission rules (`Queue`, `Replace`, `DropIfRunning`) and
//! starts the *next* task **only after** a terminal `TaskRemoved` event is observed
//! on the runtime bus. In this model, **one slot = one task name**
//! (`slot key = TaskSpec::name()`).
//!
//! ---
//!
//! ## Role in Taskvisor
//!
//! The controller accepts `ControllerSpec`, unwraps its `TaskSpec`, applies admission
//! rules (including Replace latest-wins), and delegates `add/remove` to the Supervisor.
//! It subscribes to the Bus and advances slots strictly on `TaskRemoved` to avoid
//! registry races and double starts.
//!
//! ```text
//! ┌────────────────────────────┐
//! │     Application code       │
//! │ sup.submit(ControllerSpec) │
//! └──────────────┬─────────────┘
//!                │
//!                ▼
//!        ┌──────────────────┐
//!        │    Controller    │  (per-slot admission FSM)
//!        └─────────┬────────┘
//!      unwraps & applies policy
//!                  ▼
//!        ┌──────────────────┐
//!        │     TaskSpec     │  (from ControllerSpec)
//!        └─────────┬────────┘
//!              add/remove
//!                  ▼
//!        ┌──────────────────┐
//!        │    Supervisor    │
//!        └─────────┬────────┘
//!      publishes runtime events
//!                  ▼
//!        ┌──────────────────┐   subscribes
//!        │       Bus        │◄────────────── Controller
//!        └─────────┬────────┘
//!                  ▼
//!             Task Actors
//! ```
//!
//! **Flow summary**
//! 1. Application calls `sup.submit(ControllerSpec)`.
//! 2. Controller unwraps `TaskSpec` and applies `Admission` rules.
//! 3. If accepted, controller calls `Supervisor::add_task(TaskSpec)` or requests remove.
//! 4. On terminal `TaskRemoved` (via Bus), the slot becomes `Idle` and the next queued
//!    task (if any) is started.
//!
//! ---
//!
//! ## Per-slot model
//!
//! - **Key**: `task_spec.name()` (exactly one running task per slot).
//! - **State**: `Idle | Running | Terminating`, with a FIFO queue per slot.
//! - **Replace (latest-wins)**: does **not** grow the queue; it **replaces the head**
//!   (the immediate successor). The next task actually starts **only after `TaskRemoved`**.
//!
//! ```text
//! State machine (per task name)
//!
//!   Idle ── submit → start → Running ── submit(Replace) → Terminating
//!    ▲             (Supervisor.add)                 │
//!    └─────────── on TaskRemoved ◄──────────────────┘
//!                    ├─ if queue empty → Idle
//!                    └─ if queue head exists → start head → Running
//! ```
//!
//! Queue operations
//! - **Queue**: push to tail (FIFO).
//! - **Replace**: replace head (latest-wins), not increasing depth.
//!
//! ```text
//! Queue example (head on the left):
//!   [ next, a, b ] --(Replace c)--> [ c, a, b ] --(Replace d)--> [ d, a, b ]
//!                  (head replaced)                  (head replaced)
//! ```
//!
//! ---
//!
//! ## Why gate on `TaskRemoved`
//!
//! `ActorExhausted/ActorDead` may arrive **before** full deregistration of the actor.
//! Starting the next task on those signals can race the registry and cause
//! `task_already_exists`. Gating advancement on **`TaskRemoved`** prevents double-adds.
//!
//! ---
//!
//! ## Concurrency & scalability
//!
//! - `DashMap<String, Arc<Mutex<SlotState>>>` avoids global map lock contention.
//! - Per-slot `Mutex` ensures updates to one slot don’t block others.
//!
//! ---
//!
//! ## Public surface
//!
//! - Configure via `Supervisor::builder(..).with_controller(ControllerConfig)`.
//! - Submit via `sup.submit(ControllerSpec::{queue, replace, drop_if_running}(...))`.
//! - Policies: [`ControllerAdmission`] = `Queue | Replace | DropIfRunning`.
//! - Controller emits `ControllerSubmitted`, `ControllerRejected`, and
//!   `ControllerSlotTransition` (feature `"controller"`); readable with `"logging"`’s `LogWriter`.
//!
//! ## Invariants
//! - At most **one** running task per slot.
//! - Slots advance to next task **only** after `TaskRemoved`.
//! - `Replace` is **latest-wins** (head replace); `Queue` is FIFO.

mod admission;
mod config;
mod error;
mod spec;
mod core;
mod slot;

pub use admission::ControllerAdmission;
pub use config::ControllerConfig;
pub use core::Controller;
pub use error::ControllerError;
pub use spec::ControllerSpec;