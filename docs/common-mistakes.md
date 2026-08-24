---
title: Common mistakes
description: Avoid incorrect assumptions about task results, admission, cancellation, blocking work, and side effects.
---

# Common mistakes

- Treating `run().await == Ok(())` as proof that every task succeeded.
- Treating `submit().await?` as positive slot admission.
- Using best-effort events for application decisions.
- Forgetting to observe cancellation in a resident task.
- Treating a controller slot as a registered task name.
- Running blocking or CPU-heavy work on Tokio worker threads.
- Assuming a timeout or force-abort can undo external side effects.

## Continue learning

| Resource                                          | Next step                                          |
|---------------------------------------------------|----------------------------------------------------|
| [Examples guide](../examples/README.md)           | Choose a complete runnable scenario.               |
| [API documentation](https://docs.rs/taskvisor)    | Read exact contracts for public types and methods. |
| [Benchmark guide](../benches/README.md)           | Run and interpret the Criterion suites.            |
| [Contributor map](../src/ARCHITECTURE.md)         | Follow runtime ownership and source boundaries.    |

