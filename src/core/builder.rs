use std::sync::Arc;
use tokio::sync;

use crate::{
    core::Config,
    events::Bus,
    subscribers::{Subscribe, SubscriberSet},
};
use super::{alive::AliveTracker, registry::Registry, supervisor::Supervisor};

/// Builder for constructing a Supervisor with optional features.
pub struct SupervisorBuilder {
    cfg: Config,
    subscribers: Vec<Arc<dyn Subscribe>>,

    #[cfg(feature = "controller")]
    controller_config: Option<crate::controller::ControllerConfig>,
}

impl SupervisorBuilder {
    /// Creates a new builder with the given configuration.
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            subscribers: Vec::new(),

            #[cfg(feature = "controller")]
            controller_config: None,
        }
    }

    /// Sets event subscribers for observability.
    ///
    /// Subscribers receive runtime events (task lifecycle, failures, etc.)
    /// through dedicated workers with bounded queues.
    pub fn with_subscribers(mut self, subscribers: Vec<Arc<dyn Subscribe>>) -> Self {
        self.subscribers = subscribers;
        self
    }

    /// Enables the controller with the given configuration.
    ///
    /// The controller manages task slots with admission policies
    /// (Queue, Replace, DropIfRunning).
    ///
    /// Requires the `controller` feature flag.
    #[cfg(feature = "controller")]
    pub fn with_controller(mut self, config: crate::controller::ControllerConfig) -> Self {
        self.controller_config = Some(config);
        self
    }

    /// Builds and returns the Supervisor instance.
    ///
    /// This consumes the builder and initializes all runtime components:
    /// - Event bus for broadcasting
    /// - Registry for task lifecycle management
    /// - Subscriber workers
    /// - Optional controller (if configured)
    // src/core/builder.rs
    pub fn build(self) -> Arc<Supervisor> {
        let bus = Bus::new(self.cfg.bus_capacity_clamped());
        let subs = Arc::new(SubscriberSet::new(self.subscribers, bus.clone()));
        let runtime_token = tokio_util::sync::CancellationToken::new();

        let semaphore = self
            .cfg
            .concurrency_limit()
            .map(sync::Semaphore::new)
            .map(Arc::new);

        let registry = Registry::new(bus.clone(), runtime_token.clone(), semaphore);
        let alive = Arc::new(AliveTracker::new());

        let sup = Arc::new(Supervisor::new_internal(
            self.cfg,
            bus.clone(),
            subs,
            alive,
            registry,
            runtime_token.clone(),
        ));

        #[cfg(feature = "controller")]
        if let Some(ctrl_cfg) = self.controller_config {
            let controller = crate::controller::Controller::new(ctrl_cfg, &sup, bus.clone());

            let _ = sup.controller.set(Arc::clone(&controller));
            controller.run(runtime_token.clone());
        }
        sup
    }
}
