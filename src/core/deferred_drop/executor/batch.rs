//! Drops one retained value group and releases or retires its capacity.
//!
//! `DropBundle` becomes [`DropBatch`] when submitted to `DropExecutor`.
//! A worker calls [`DropBatch::run`] outside the queue lock.
//! The batch owns panic containment and the final permit disposition.

use std::{
    any::Any,
    panic::{AssertUnwindSafe, catch_unwind},
};

use super::super::{
    bundle::{DropJob, PanicReporter},
    capacity::OwnershipPermit,
};

/// Terminal destructor jobs paired with their charged permit.
///
/// The permit is declared before user jobs.
/// If an unexecuted batch is dropped, admission closes before any job is retained permanently.
pub(in crate::core::deferred_drop) struct DropBatch {
    /// Permit released or retired after the batch runs.
    permit: Option<OwnershipPermit>,
    /// First destructor job.
    retained: Option<DropJob>,
    /// Second optional destructor job.
    undelivered_outcome: Option<DropJob>,
    /// Third optional destructor job.
    auxiliary: Option<DropJob>,
    /// One-shot reporter consumed by the first destructor panic.
    panic_reporter: Option<PanicReporter>,
    /// Forces permit retirement even when every job returns.
    poisoned: bool,
}

impl DropBatch {
    /// Accepts all values collected by one terminal bundle.
    pub(in crate::core::deferred_drop) fn new(
        permit: OwnershipPermit,
        retained: DropJob,
        undelivered_outcome: Option<DropJob>,
        auxiliary: Option<DropJob>,
        panic_reporter: Option<PanicReporter>,
        poisoned: bool,
    ) -> Self {
        Self {
            permit: Some(permit),
            retained: Some(retained),
            undelivered_outcome,
            auxiliary,
            panic_reporter,
            poisoned,
        }
    }

    /// Runs every present destructor even when an earlier destructor panics.
    ///
    /// Each job has its own panic boundary. The first panic may use the diagnostic callback.
    /// Every panic payload is retained permanently before execution continues.
    /// Clean, unpoisoned completion returns the permit; any panic or poison retires it.
    pub(super) fn run(mut self) {
        let mut clean = true;
        for job in [
            self.retained.take(),
            self.undelivered_outcome.take(),
            self.auxiliary.take(),
        ]
        .into_iter()
        .flatten()
        {
            if let Err(payload) = catch_unwind(AssertUnwindSafe(job)) {
                clean = false;
                let message = panic_message(&payload);
                std::mem::forget(payload);
                if let Some(report) = self.panic_reporter.take()
                    && let Err(report_panic) = catch_unwind(AssertUnwindSafe(|| report(message)))
                {
                    std::mem::forget(report_panic);
                }
            }
        }
        if clean && !self.poisoned {
            drop(self.permit.take());
        } else if let Some(permit) = self.permit.take() {
            permit.retire();
        }
    }
}

/// Extracts a string message without consuming the panic payload.
fn panic_message(payload: &Box<dyn Any + Send>) -> String {
    payload
        .downcast_ref::<&'static str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned())
}

impl Drop for DropBatch {
    /// Closes admission and permanently retains every unexecuted job.
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            permit.close_without_release();
        }
        for job in [
            self.retained.take(),
            self.undelivered_outcome.take(),
            self.auxiliary.take(),
        ]
        .into_iter()
        .flatten()
        {
            std::mem::forget(job);
        }
    }
}
