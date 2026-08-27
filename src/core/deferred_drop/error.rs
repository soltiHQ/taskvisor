//! Reports failures to start cleanup workers or reserve cleanup capacity.

use std::{fmt, io, num::NonZeroUsize};

/// Failure to configure or start a domain's worker set.
#[derive(Debug)]
pub(crate) struct DropStartError {
    /// Zero-based failed worker index, or zero for invalid configuration.
    pub(super) worker: usize,
    /// Configuration, thread creation, or readiness-handshake error.
    pub(super) source: io::Error,
}

impl DropStartError {
    /// Records a configuration or worker startup failure.
    pub(super) fn new(worker: usize, source: io::Error) -> Self {
        Self { worker, source }
    }

    /// Failed worker index, or zero for invalid configuration.
    pub(crate) const fn worker(&self) -> usize {
        self.worker
    }

    /// Returns the underlying startup error.
    #[cfg(test)]
    pub(super) fn into_source(self) -> io::Error {
        self.source
    }

    /// Category of the underlying startup error.
    pub(crate) fn source_kind(&self) -> io::ErrorKind {
        self.source.kind()
    }

    /// Operating-system error code when one is available.
    pub(crate) fn raw_os_error(&self) -> Option<i32> {
        self.source.raw_os_error()
    }
}

impl fmt::Display for DropStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "failed to start destructor-isolation worker {}: {}",
            self.worker, self.source
        )
    }
}

impl std::error::Error for DropStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Ownership request rejected by the supervisor-local capacity broker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DropCapacityError {
    /// Configured ownership limit, or `None` when an unlimited domain closed.
    pub(super) limit: Option<NonZeroUsize>,
}

impl DropCapacityError {
    /// Records the configured limit when one applies to the rejection.
    pub(super) const fn new(limit: Option<NonZeroUsize>) -> Self {
        Self { limit }
    }

    /// Configured ownership limit.
    ///
    /// `None` means an unlimited domain rejected admission because it closed.
    pub(crate) const fn limit(self) -> Option<NonZeroUsize> {
        self.limit
    }
}

/// Reason a domain could not create an ownership reservation.
#[derive(Debug)]
pub(crate) enum DropAdmissionError {
    /// The required worker set did not start transactionally.
    Start(DropStartError),
    /// The capacity broker could not grant the request.
    Capacity(DropCapacityError),
}
