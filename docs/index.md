---
title: Taskvisor user guide
description: Choose the Taskvisor workflow that fits an application and follow its production boundaries.
---

# Taskvisor user guide

This guide explains how to use Taskvisor in an application and how to choose between its public workflows.
For exact method signatures, error variants, and edge-case contracts, use the [API documentation](https://docs.rs/taskvisor).

Taskvisor is an in-process runtime. Tasks, queued submissions, task IDs, events, and watched outcomes do not survive process exit.
Use durable external storage when work must resume after a restart.

- New to Taskvisor? Complete the [Quick start](quick-start.md).
- Looking for a complete program? Browse the [Taskvisor examples](../examples/README.md).
- Changing Taskvisor itself? Start with the [contributor map](../src/ARCHITECTURE.md).

## In this guide

- Start: [quick start](quick-start.md), [mental model](mental-model.md), [installation](installation.md), [task definition](defining-tasks.md), and [task behavior](lifecycle-policies.md).
- Run: [supervisor entry points](running-and-managing.md), [runtime management](managing-tasks.md), then [cancellation and shutdown](cancellation-and-shutdown.md).
- Extend: [outcomes and events](outcomes-and-events.md), [per-key coordination](keyed-admission.md), and [configuration](configuration.md).
- Deploy: [production boundaries](production-boundaries.md) and [common mistakes](common-mistakes.md).
