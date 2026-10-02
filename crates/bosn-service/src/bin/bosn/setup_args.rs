//! Parsing setup plan/prepare/task/ensure arguments, and their failure output.

use super::*;

pub(crate) fn setup_prepare_failure() -> ! {
    eprintln!("bosn setup prepare: submission failed");
    std::process::exit(1)
}

pub(crate) fn setup_task_failure(json: bool) -> ! {
    if json {
        println!(
            "{}",
            json!({"action": "setup_task", "error": "request failed"})
        );
    } else {
        eprintln!("bosn setup task: submission failed");
    }
    std::process::exit(1)
}

pub(crate) fn setup_ensure_failure(json: bool) -> ! {
    if json {
        println!(
            "{}",
            json!({"action": "setup_ensure", "error": "request failed"})
        );
    } else {
        eprintln!("bosn setup ensure: submission failed");
    }
    std::process::exit(1)
}

pub(crate) struct PlanInvocation {
    pub(crate) request: SetupPlanRequest,
    pub(crate) json: bool,
}

pub(crate) struct PrepareInvocation {
    pub(crate) state_dir: PathBuf,
    pub(crate) request: SetupPrepareRequest,
    pub(crate) json: bool,
}

pub(crate) struct TaskInvocation {
    pub(crate) state_dir: PathBuf,
    pub(crate) request: SetupTaskJobRequest,
    pub(crate) json: bool,
}

pub(crate) struct EnsureInvocation {
    pub(crate) state_dir: PathBuf,
    pub(crate) request: SetupEnsureJobRequest,
    pub(crate) json: bool,
}

/// The native plan command requires every stateful choice to be written on the
/// command line.  This prevents an unnoticed cache read or ambient workspace
/// from becoming an implicit apply input.
pub(crate) fn parse_plan_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<PlanInvocation, ()> {
    let mut state_dir = None;
    let mut workspace = None;
    let mut locator = None;
    let mut policy = None;
    let mut json = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once(&mut state_dir, arguments.next()),
            "--workspace" => set_once(&mut workspace, arguments.next()),
            "--config" => set_once(&mut locator, arguments.next()),
            "--refresh" => set_once(&mut policy, Some(SetupAcquirePolicy::OnlineRefresh)),
            "--offline" => set_once(&mut policy, Some(SetupAcquirePolicy::OfflineCacheOnly)),
            "--json" if !json => {
                json = true;
                Ok(())
            }
            _ => Err(()),
        }?;
    }
    Ok(PlanInvocation {
        request: SetupPlanRequest {
            state_dir: PathBuf::from(state_dir.ok_or(())?),
            workspace: PathBuf::from(workspace.ok_or(())?),
            locator: locator.ok_or(())?.into_string().map_err(|_| ())?,
            policy: policy.ok_or(())?,
        },
        json,
    })
}

/// Parse every preparation input before creating a runtime or opening daemon
/// IPC. The daemon repeats equivalent wire validation; this boundary keeps bad
/// local invocations from making any state or daemon contact at all.
pub(crate) fn parse_prepare_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<PrepareInvocation, ()> {
    let mut state_dir = None;
    let mut workspace = None;
    let mut config = None;
    let mut policy = None;
    let mut deadline_ms = None;
    let mut output_limit = None;
    let mut json = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once(&mut state_dir, arguments.next()),
            "--workspace" => {
                set_once_parsed(&mut workspace, arguments.next(), parse_setup_request_text)
            }
            "--config" => set_once_parsed(&mut config, arguments.next(), parse_setup_config),
            "--refresh" => set_once(&mut policy, Some(SetupPreparePolicy::Refresh)),
            "--offline" => set_once(&mut policy, Some(SetupPreparePolicy::Offline)),
            "--deadline-ms" => set_once_parsed(&mut deadline_ms, arguments.next(), |value| {
                let value = parse_u64(value)?;
                (1..=SETUP_PREPARE_MAX_DEADLINE_MS)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--output-limit" => set_once_parsed(&mut output_limit, arguments.next(), |value| {
                let value = parse_usize(value)?;
                (1..=SETUP_PREPARE_MAX_OUTPUT_LIMIT)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--json" if !json => {
                json = true;
                Ok(())
            }
            _ => Err(()),
        }?;
    }
    Ok(PrepareInvocation {
        state_dir: PathBuf::from(state_dir.ok_or(())?),
        request: SetupPrepareRequest {
            workspace: PathBuf::from(workspace.ok_or(())?),
            config: config.ok_or(())?,
            policy: policy.ok_or(())?,
            deadline: Duration::from_millis(deadline_ms.ok_or(())?),
            output_limit: output_limit.ok_or(())?,
        },
        json,
    })
}

/// Parse every task-submission input before constructing a runtime or opening
/// daemon IPC. In particular, the only executable selection is a setup
/// document task name; callers cannot provide a command, mounts, environment,
/// work directory, container, or state override through the request.
pub(crate) fn parse_task_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<TaskInvocation, ()> {
    let mut state_dir = None;
    let mut workspace = None;
    let mut config = None;
    let mut policy = None;
    let mut task_name = None;
    let mut deadline_ms = None;
    let mut output_limit = None;
    let mut json = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--workspace" => {
                set_once_parsed(&mut workspace, arguments.next(), parse_setup_request_text)
            }
            "--config" => set_once_parsed(&mut config, arguments.next(), parse_setup_config),
            "--refresh" => set_once(&mut policy, Some(SetupPreparePolicy::Refresh)),
            "--offline" => set_once(&mut policy, Some(SetupPreparePolicy::Offline)),
            "--task" => set_once_parsed(&mut task_name, arguments.next(), parse_setup_task_name),
            "--deadline-ms" => set_once_parsed(&mut deadline_ms, arguments.next(), |value| {
                let value = parse_u64(value)?;
                (1..=SETUP_PREPARE_MAX_DEADLINE_MS)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--output-limit" => set_once_parsed(&mut output_limit, arguments.next(), |value| {
                let value = parse_usize(value)?;
                (1..=SETUP_PREPARE_MAX_OUTPUT_LIMIT)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--json" if !json => {
                json = true;
                Ok(())
            }
            _ => Err(()),
        }?;
    }
    Ok(TaskInvocation {
        state_dir: state_dir.ok_or(())?,
        request: SetupTaskJobRequest {
            workspace: PathBuf::from(workspace.ok_or(())?),
            config: config.ok_or(())?,
            policy: policy.ok_or(())?,
            task_name: task_name.ok_or(())?,
            deadline: Duration::from_millis(deadline_ms.ok_or(())?),
            output_limit: output_limit.ok_or(())?,
        },
        json,
    })
}

/// Parse every ensure-submission input before constructing a runtime or
/// resolving the local IPC endpoint. The request intentionally has no app
/// command, image, container, mount, environment, work-directory, network,
/// privilege, label, state override, task, or raw Docker arguments.
pub(crate) fn parse_ensure_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<EnsureInvocation, ()> {
    let mut state_dir = None;
    let mut workspace = None;
    let mut config = None;
    let mut policy = None;
    let mut deadline_ms = None;
    let mut output_limit = None;
    let mut json = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--workspace" => {
                set_once_parsed(&mut workspace, arguments.next(), parse_setup_request_text)
            }
            "--config" => set_once_parsed(&mut config, arguments.next(), parse_setup_config),
            "--refresh" => set_once(&mut policy, Some(SetupPreparePolicy::Refresh)),
            "--offline" => set_once(&mut policy, Some(SetupPreparePolicy::Offline)),
            "--deadline-ms" => set_once_parsed(&mut deadline_ms, arguments.next(), |value| {
                let value = parse_u64(value)?;
                (1..=SETUP_PREPARE_MAX_DEADLINE_MS)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--output-limit" => set_once_parsed(&mut output_limit, arguments.next(), |value| {
                let value = parse_usize(value)?;
                (1..=SETUP_PREPARE_MAX_OUTPUT_LIMIT)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--json" if !json => {
                json = true;
                Ok(())
            }
            _ => Err(()),
        }?;
    }
    Ok(EnsureInvocation {
        state_dir: state_dir.ok_or(())?,
        request: SetupEnsureJobRequest {
            workspace: PathBuf::from(workspace.ok_or(())?),
            config: config.ok_or(())?,
            policy: policy.ok_or(())?,
            deadline: Duration::from_millis(deadline_ms.ok_or(())?),
            output_limit: output_limit.ok_or(())?,
        },
        json,
    })
}
