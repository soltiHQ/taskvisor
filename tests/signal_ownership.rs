//! Process-isolated regression tests for implicit OS signal ownership.

#![cfg(unix)]

use std::{os::unix::process::ExitStatusExt, process::Command, time::Duration};

use taskvisor::prelude::*;

const CHILD_ENV: &str = "TASKVISOR_PLAIN_RUN_SIGNAL_CHILD";

#[test]
fn plain_run_does_not_install_process_signal_handlers() {
    if std::env::var_os(CHILD_ENV).is_some() {
        child_runs_supervisor_then_sends_sigterm();
        return;
    }

    let status = Command::new(std::env::current_exe().expect("the test binary path must exist"))
        .arg("--exact")
        .arg("plain_run_does_not_install_process_signal_handlers")
        .arg("--nocapture")
        .env(CHILD_ENV, "1")
        .status()
        .expect("the isolated signal test must start");

    assert_eq!(
        status.signal(),
        Some(15),
        "plain run must leave SIGTERM at its process-default disposition; child status: {status}"
    );
}

fn child_runs_supervisor_then_sends_sigterm() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the child Tokio runtime must build");
    runtime.block_on(async {
        let task: TaskRef = TaskFn::arc("natural", |_ctx| async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(())
        });
        let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
        supervisor
            .run(vec![TaskSpec::once(task)])
            .await
            .expect("plain run must finish naturally");
    });

    // Use the POSIX shell builtin instead of requiring an external `kill`
    // executable, which minimal CI images may not install.
    let kill = Command::new("/bin/sh")
        .arg("-c")
        .arg("kill -TERM \"$1\"")
        .arg("taskvisor-signal-test")
        .arg(std::process::id().to_string())
        .status()
        .expect("the child must be able to invoke the POSIX shell");
    assert!(kill.success(), "the shell builtin must accept SIGTERM");

    std::thread::sleep(Duration::from_millis(250));
    panic!("the child survived SIGTERM after plain Supervisor::run");
}
