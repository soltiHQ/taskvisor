/// # Per-task admission policy
///
/// Controller treats tasks as **slots** identified by `name`.
/// At any given time **one** task may run in a slot.
/// When a new request for the same slot arrives, the admission policy decides what to do.
///
/// ## Variants
/// - `DropIfRunning` — If the slot is already running, **ignore** the new request.
/// - `Replace` — **Stop** the running task (cancel/remove) and start the new one.
/// - `Queue` — **Enqueue** the new request (FIFO). It will start automatically
///   when the current task completes or is cancelled.
///
/// ## Invariants
/// - Tasks within the same slot never run in parallel (use dynamic names if you need parallel execution).
/// - Queued requests are executed strictly in submission order.
#[derive(Clone, Debug)]
pub enum Admission {
    // Skip task if already running policy.
    DropIfRunning,
    // Stop current task (if execution in progress) and start new one.
    Replace,
    // Put task in queue (executed strictly in submission order).
    Queue,
}