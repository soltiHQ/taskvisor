//! Controller adapter for the process-wide bounded task-metadata executor.

use std::sync::Arc;

use tokio::sync::oneshot;

use crate::{
    ControllerSpec,
    core::{
        deferred_drop::OwnedTask,
        task_metadata::{self, TaskNameSnapshot as CommonTaskNameSnapshot},
    },
    identity::TaskId,
};

pub(super) type TaskNameSnapshot = CommonTaskNameSnapshot<ControllerSpec>;

/// Completion returned to the serialized controller loop.
pub(super) struct MetadataResult {
    pub(super) id: TaskId,
    /// `None` means the pending submission was canceled or the fixed worker
    /// set became unavailable before producing metadata.
    pub(super) snapshot: Option<TaskNameSnapshot>,
}

pub(super) fn snapshot_task_name(
    owned: OwnedTask<ControllerSpec>,
) -> Result<oneshot::Receiver<TaskNameSnapshot>, Box<OwnedTask<ControllerSpec>>> {
    task_metadata::snapshot_task_name(owned, |spec| Arc::<str>::from(spec.task_spec().name()))
}
