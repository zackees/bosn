use bosn_core::{
    PolicyDefaults, PolicyOrigin, parse_machine_policy_toml, resolve_app_policy,
    resolve_machine_policy,
};

#[test]
fn machine_toml_precedence_types_and_origins_are_explicit() {
    let file_only = parse_machine_policy_toml(
        "[policy]\nrun_max_duration=10.5\n",
        PolicyDefaults::for_cpu_count(Some(8)),
        std::iter::empty::<(&str, &str)>(),
        std::iter::empty::<(&str, &str)>(),
    )
    .unwrap();
    assert_eq!(
        file_only.origin("run_max_duration"),
        Some(PolicyOrigin::MachineFile)
    );
    let environment = parse_machine_policy_toml(
        "[policy]\nrun_max_duration=10\n",
        PolicyDefaults::for_cpu_count(Some(8)),
        [("run_max_duration", "9")],
        [],
    )
    .unwrap();
    assert_eq!(
        environment.origin("run_max_duration"),
        Some(PolicyOrigin::MachineEnvironment)
    );
    let policy = parse_machine_policy_toml(
        "[policy]\nrun_max_duration=10\n",
        PolicyDefaults::for_cpu_count(Some(8)),
        [("run_max_duration", "9")],
        [("run_max_duration", "8")],
    )
    .unwrap();
    assert_eq!(policy.get("run_max_duration"), Some(8.0));
    assert_eq!(
        policy.origin("run_max_duration"),
        Some(PolicyOrigin::MachineFlag)
    );
    for invalid in [
        "[policy]\nrun_max_duration=true",
        "[policy]\nrun_max_duration=nan",
        "[policy]\nunknown=1",
        "[policy]\nrun_max_duration=1\nother=2",
        "[policy]\nrun_max_duration=1\n[other]\nx=1",
    ] {
        assert!(
            parse_machine_policy_toml(
                invalid,
                PolicyDefaults::for_cpu_count(None),
                std::iter::empty::<(&str, &str)>(),
                std::iter::empty::<(&str, &str)>(),
            )
            .is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn app_policy_does_not_mutate_machine_policy_or_override_global_knobs() {
    let machine = resolve_machine_policy(
        PolicyDefaults::for_cpu_count(Some(4)),
        [("run_max_duration", "12")],
        [("run_max_duration", "11")],
        [("run_max_duration", "10")],
    )
    .unwrap();
    let app = resolve_app_policy(&machine, [("run_max_duration", "9")]).unwrap();
    assert_eq!(machine.get("run_max_duration"), Some(10.0));
    assert_eq!(
        machine.origin("run_max_duration"),
        Some(PolicyOrigin::MachineFlag)
    );
    assert_eq!(app.get("run_max_duration"), Some(9.0));
    assert_eq!(app.machine().get("run_max_duration"), Some(10.0));
    assert!(resolve_app_policy(&machine, [("max_builds", "1")]).is_err());
    assert!(resolve_app_policy(&machine, [("run_max_duration", "11")]).is_err());
}
