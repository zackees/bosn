use super::{
    CommandError, CommandResult, DockerDoctorState, GuestScpCommand, GuestSshCommand, doctor_report,
};
use std::path::PathBuf;

#[test]
fn docker_engine_debug_never_prints_environment_values() {
    let engine = super::DockerEngine::docker().env("GITHUB_TOKEN", "canary-308-secret");
    let rendered = format!("{engine:?}");
    assert!(rendered.contains("GITHUB_TOKEN"));
    assert!(!rendered.contains("canary-308-secret"));
}

#[test]
fn guest_ssh_command_ignores_ambient_config_and_fixes_loopback_target() {
    let command = GuestSshCommand {
        user: "runner".into(),
        port: 2222,
        identity_file: PathBuf::from("/state/guest-ssh/key"),
        command: "echo declared".into(),
    };
    let args = command
        .args()
        .into_iter()
        .map(|value| value.into_string().unwrap())
        .collect::<Vec<_>>();
    assert!(args.windows(2).any(|pair| pair == ["-F", "/dev/null"]));
    assert!(args.windows(2).any(|pair| pair == ["-p", "2222"]));
    assert!(args.contains(&"runner@127.0.0.1".into()));
    assert!(args.contains(&"BatchMode=yes".into()));
    assert!(args.contains(&"IdentitiesOnly=yes".into()));
    assert_eq!(args.last().unwrap(), "echo declared");
    assert!(!args.iter().any(|value| value.contains("ProxyCommand")));
}

#[test]
fn guest_scp_command_is_fixed_to_loopback_and_uses_scp_port_spelling() {
    let command = GuestScpCommand {
        user: "runner".into(),
        port: 2222,
        identity_file: PathBuf::from("/state/guest-ssh/key"),
        source: PathBuf::from("/workspace/out/archive.tar.zst"),
        destination: "~/archive.tar.zst".into(),
    };
    let args = command
        .args()
        .into_iter()
        .map(|value| value.into_string().unwrap())
        .collect::<Vec<_>>();
    assert!(args.windows(2).any(|pair| pair == ["-F", "/dev/null"]));
    assert!(args.windows(2).any(|pair| pair == ["-P", "2222"]));
    assert!(args.contains(&"/workspace/out/archive.tar.zst".into()));
    assert_eq!(args.last().unwrap(), "runner@127.0.0.1:~/archive.tar.zst");
    assert!(args.contains(&"IdentitiesOnly=yes".into()));
    assert!(!args.iter().any(|value| value.contains("ProxyCommand")));
}

#[test]
fn error_display_claims_reaping_only_after_confirmed_cleanup() {
    assert_eq!(
        CommandError::Deadline {
            reaped_pid: None,
            cleanup: None
        }
        .to_string(),
        "Docker CLI exceeded its deadline"
    );
    assert_eq!(
        CommandError::Cancelled {
            reaped_pid: Some(7),
            cleanup: None
        }
        .to_string(),
        "Docker CLI was cancelled and was reaped"
    );
    assert_eq!(
        CommandError::Deadline {
            reaped_pid: None,
            cleanup: Some("wait failed".into())
        }
        .to_string(),
        "Docker CLI exceeded its deadline; cleanup failed: wait failed"
    );
}

#[test]
fn all_output_consumer_states_have_specific_display() {
    assert_eq!(
        CommandError::OutputLimit {
            limit: 3,
            reaped_pid: Some(7),
            cleanup: Some("wait failed".into())
        }
        .to_string(),
        "Docker CLI output exceeded 3 bytes and was reaped; cleanup failed: wait failed"
    );
    assert_eq!(
        CommandError::OutputCompletion {
            detail: "fault".into(),
            reaped_pid: None,
            cleanup: Some("wait failed".into())
        }
        .to_string(),
        "Docker CLI output did not complete cleanly: fault; cleanup failed: wait failed"
    );
    assert_eq!(
        CommandError::OutputConsumerSlow {
            reaped_pid: None,
            cleanup: None
        }
        .to_string(),
        "Docker CLI output consumer was too slow"
    );
    assert_eq!(
        CommandError::OutputConsumerClosed {
            reaped_pid: Some(7),
            cleanup: None
        }
        .to_string(),
        "Docker CLI output consumer closed and was reaped"
    );
}

#[test]
fn doctor_result_is_structured_and_never_retains_raw_output() {
    let ready = doctor_report(Ok(CommandResult {
        exit_code: 0,
        stdout: b"29.0.1|29.0.1\n".to_vec(),
        stderr: b"secret engine warning".to_vec(),
    }));
    assert_eq!(ready.state, DockerDoctorState::Ready);
    assert_eq!(ready.client_version.as_deref(), Some("29.0.1"));
    assert_eq!(ready.server_version.as_deref(), Some("29.0.1"));

    let invalid = doctor_report(Ok(CommandResult {
        exit_code: 0,
        stdout: b"version|value|extra".to_vec(),
        stderr: Vec::new(),
    }));
    assert_eq!(invalid.state, DockerDoctorState::InvalidResponse);
    assert_eq!(invalid.client_version, None);

    let limited = doctor_report(Err(CommandError::OutputLimit {
        limit: 1,
        reaped_pid: None,
        cleanup: None,
    }));
    assert_eq!(limited.state, DockerDoctorState::OutputLimit);
    assert_eq!(limited.server_version, None);
}

#[test]
fn only_dockers_no_such_answer_reports_missing() {
    let result = |exit_code, stderr: &str| CommandResult {
        exit_code,
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    };
    assert!(result(1, "Error response from daemon: No such container: x").reports_missing());
    assert!(result(1, "Error: No such object: x").reports_missing());
    assert!(result(1, "Error response from daemon: get x: no such volume").reports_missing());
    assert!(
        !result(
            1,
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock."
        )
        .reports_missing()
    );
    assert!(
        !result(
            1,
            "permission denied while trying to connect to the Docker daemon socket"
        )
        .reports_missing()
    );
    assert!(!result(125, "No such container: x").reports_missing());
}

#[test]
fn a_failure_beside_a_missing_object_is_not_absence() {
    let result = CommandResult {
        exit_code: 1,
        stdout: b"[]".to_vec(),
        stderr: b"Error: No such object: a\nCannot connect to the Docker daemon\n".to_vec(),
    };
    assert!(!result.reports_missing());
    let only_missing = CommandResult {
        exit_code: 1,
        stdout: b"[{\"Id\":\"b\"}]".to_vec(),
        stderr: b"Error: No such object: a\nError: No such object: c\n".to_vec(),
    };
    assert!(only_missing.reports_missing());
    assert_eq!(
        super::managed_reads::inspect_read(only_missing, "docker inspect").document(),
        Some("[{\"Id\":\"b\"}]")
    );
}
