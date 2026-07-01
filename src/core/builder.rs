//! # Supervisor builder.
//!
//! Builds a [`Supervisor`](crate::Supervisor) from [`SupervisorConfig`] and optional runtime extensions.
//!
//! ## Example
//!
//! ```rust
//! use taskvisor::{SupervisorBuilder, SupervisorConfig};
//!
//! let supervisor = SupervisorBuilder::new(SupervisorConfig::default()).build();
//! ```

use std::sync::Arc;
use tokio::{sync, sync::mpsc};

use super::{
    alive::AliveTracker, registry::Registry, runtime::SupervisorCore, supervisor::Supervisor,
};
use crate::{
    core::SupervisorConfig,
    events::Bus,
    subscribers::{Subscribe, SubscriberSet},
};

/// Builder for constructing a [`Supervisor`](crate::Supervisor).
///
/// Use this when you need to customize runtime config, subscribers, or optional feature-backed components before starting the supervisor.
///
/// The produced supervisor is not started yet.
/// Calling `build` only creates the runtime graph and stores the pieces that will be used later by `Supervisor::run` or `Supervisor::serve`.
///
/// # Also
///
/// - [`Supervisor`](crate::Supervisor) - runtime facade produced by this builder
/// - [`SupervisorConfig`] - runtime defaults and limits
/// - [`Subscribe`](crate::Subscribe) - event subscriber trait
pub struct SupervisorBuilder {
    cfg: SupervisorConfig,
    subscribers: Vec<Arc<dyn Subscribe>>,

    #[cfg(feature = "controller")]
    controller_config: Option<crate::controller::ControllerConfig>,
}

impl SupervisorBuilder {
    /// Creates a new builder with the given runtime configuration.
    pub fn new(cfg: SupervisorConfig) -> Self {
        Self {
            cfg,
            subscribers: Vec::new(),

            #[cfg(feature = "controller")]
            controller_config: None,
        }
    }

    /// Sets event subscribers for runtime observability.
    ///
    /// Each subscriber gets its own bounded queue and worker task.
    /// The workers are created during [`build`](Self::build), and events are delivered after the supervisor starts its runtime listener.
    ///
    /// Passing a new list replaces any subscribers previously set on this builder.
    pub fn with_subscribers(mut self, subscribers: Vec<Arc<dyn Subscribe>>) -> Self {
        self.subscribers = subscribers;
        self
    }

    /// Enables the controller with the given configuration.
    ///
    /// The controller adds slot-based task admission on top of the core supervisor.
    /// It can queue, replace, or drop submissions depending on the configured slot policy.
    ///
    /// Available only with the `controller` feature.
    #[cfg(feature = "controller")]
    pub fn with_controller(mut self, config: crate::controller::ControllerConfig) -> Self {
        self.controller_config = Some(config);
        self
    }

    /// Builds the supervisor.
    ///
    /// This consumes the builder and creates all runtime components.
    ///
    /// The supervisor is returned in a stopped state.
    /// Task execution starts when the caller runs or serves the supervisor.
    pub fn build(self) -> Arc<Supervisor> {
        let bus = Bus::new(self.cfg.bus_capacity_clamped());
        let subs = Arc::new(SubscriberSet::new(self.subscribers, bus.clone()));
        let runtime_token = tokio_util::sync::CancellationToken::new();

        let semaphore = self
            .cfg
            .max_concurrent
            .map(|n| Arc::new(sync::Semaphore::new(n.get())));

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let registry = Registry::new(
            bus.clone(),
            runtime_token.clone(),
            semaphore,
            self.cfg.grace,
            cmd_rx,
        );
        let alive = Arc::new(AliveTracker::new());

        let core = SupervisorCore::new_internal(
            self.cfg,
            bus.clone(),
            subs,
            alive,
            registry,
            runtime_token.clone(),
            cmd_tx,
        );

        #[cfg(feature = "controller")]
        let controller = self
            .controller_config
            .map(|ctrl_cfg| crate::controller::Controller::new(ctrl_cfg, &core, bus.clone()));

        Supervisor::from_parts(
            core,
            #[cfg(feature = "controller")]
            controller,
            #[cfg(feature = "controller")]
            runtime_token,
        )
    }
}
