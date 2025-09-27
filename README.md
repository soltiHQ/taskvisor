# taskvisor

> Event-driven async task orchestration library with supervision, backoff, and observers.

[![Crates.io](https://img.shields.io/crates/v/taskvisor.svg)](https://crates.io/crates/taskvisor)
[![Docs.rs](https://docs.rs/taskvisor/badge.svg)](https://docs.rs/taskvisor)
---

## Overview
**taskvisor** is an event-driven supervisor for asynchronous tasks.  
It provides **restart policies**, **backoff strategies**, **timeouts**, and **graceful shutdown**,  
while exposing events via the [`Observer`] trait for logging, metrics, or custom monitoring.
---

## Features
- 🛑 Graceful shutdown on OS signals (SIGINT, SIGTERM)
- 🔄 Restart policies (`Never`, `OnFailure`, `Always`)
- ⏱ Configurable timeouts and grace periods
- 📡 Event bus with pluggable observers
- ⏳ Exponential backoff strategies
- 🚦 Supervision of async tasks
---

## Quick start
```rust,no_run
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use taskvisor::{
    BackoffStrategy, 
    Config, 
    RestartPolicy, 
    Supervisor, 
    TaskFn, 
    TaskRef, 
    TaskSpec, 
    LoggerObserver,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut cfg = Config::default();
    cfg.max_concurrent = 2;
    cfg.grace = Duration::from_secs(5);

    let sup = Supervisor::new(cfg.clone(), LoggerObserver);

    let t1: TaskRef = TaskFn::arc("ticker", |ctx: CancellationToken| async move {
        while !ctx.is_cancelled() {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Ok(())
    });

    let spec = TaskSpec::new(
        t1,
        RestartPolicy::Never,
        BackoffStrategy::default(),
        Some(Duration::from_secs(2)),
    );

    sup.run(vec![spec]).await?;
    Ok(())
}