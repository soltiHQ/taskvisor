//! Converts public task selectors into one management-operation target.

use std::sync::Arc;

use crate::TaskId;

/// Selects a task by controller-aware identity or by registered name.
///
/// Identity operations can address controller work that has not reached the registry yet.
/// Name operations resolve only current registry membership because queued controller work does not own a registered name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TaskTarget {
    /// Selects one task or controller submission by its process-local identity.
    Id(TaskId),
    /// Selects the current registered owner of a task name.
    Name(Arc<str>),
}

impl From<TaskId> for TaskTarget {
    fn from(id: TaskId) -> Self {
        Self::Id(id)
    }
}

impl From<Arc<str>> for TaskTarget {
    fn from(name: Arc<str>) -> Self {
        Self::Name(name)
    }
}

impl From<&Arc<str>> for TaskTarget {
    fn from(name: &Arc<str>) -> Self {
        Self::Name(Arc::clone(name))
    }
}

impl From<Box<str>> for TaskTarget {
    fn from(name: Box<str>) -> Self {
        Self::Name(Arc::from(name))
    }
}

impl From<String> for TaskTarget {
    fn from(name: String) -> Self {
        Self::Name(Arc::from(name))
    }
}

impl From<&String> for TaskTarget {
    fn from(name: &String) -> Self {
        Self::Name(Arc::from(name.as_str()))
    }
}

impl From<&str> for TaskTarget {
    fn from(name: &str) -> Self {
        Self::Name(Arc::from(name))
    }
}
