//! Opt-in command contract test using an actual act2 retention binary.
use super::cache_cohort::CacheRoute;
use super::cache_cohort::Namespace;
use super::cache_policy::CachePolicy;
use super::engine::{ActInvocation, ENGINE_CACHE, ENGINE_WORK};

const SCRIPT: &str = r#"
import json, pathlib, queue, signal, subprocess, sys, tempfile, threading
binary, server_json, watcher_json, original_root = sys.argv[1:]
with tempfile.TemporaryDirectory() as directory:
    root = str(pathlib.Path(directory) / 'cohort')
    def relocate(values):
        return [value.replace(original_root, root) for value in json.loads(values)]
    server = subprocess.run([binary, 'cache', 'audit', *relocate(server_json)],
                            capture_output=True, text=True, timeout=10)
    assert server.returncode == 0, (server.stdout, server.stderr)
    audit = json.loads(server.stdout)
    assert not audit['partial'] and audit['status'] == 'missing', audit
    assert audit['entry_count'] is None and audit['archive_bytes'] is None, audit
    watcher = subprocess.Popen([binary, *relocate(watcher_json)],
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        lines = queue.Queue()
        threading.Thread(target=lambda: lines.put(watcher.stdout.readline()), daemon=True).start()
        report = json.loads(lines.get(timeout=10))
        assert report['partial'] and report['budget_bytes'] == 200, report
        assert report['budget_met'] is None, report
        watcher.send_signal(signal.SIGINT)
        output, errors = watcher.communicate(timeout=10)
        assert watcher.returncode == 0, (watcher.returncode, output, errors)
    finally:
        if watcher.poll() is None:
            watcher.kill()
            watcher.communicate(timeout=10)
"#;

#[test]
#[ignore = "requires BOSN_ACT_RETENTION_TEST_BINARY pointing to a verified act2 build"]
fn cohort_commands_accept_policy_report_unknown_and_stop_on_first_interrupt() {
    let binary = std::env::var("BOSN_ACT_RETENTION_TEST_BINARY").unwrap();
    let policy: CachePolicy = toml::from_str(
        "repository_max_bytes=100\naggregate_max_bytes=200\nmax_age_secs=3600\nunused_age_secs=1800\nmaintenance_interval_secs=60\n",
    ).unwrap();
    let namespace = Namespace::parse("0123456789abcdef").unwrap();
    let result = std::process::Command::new("python3")
        .args([
            "-c",
            SCRIPT,
            &binary,
            &serde_json::to_string(&policy.server_args(&namespace)).unwrap(),
            &serde_json::to_string(&policy.maintenance_args()).unwrap(),
            &super::cache_cohort::root(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
#[ignore = "requires BOSN_ACT_RETENTION_TEST_BINARY pointing to verified act2.7"]
fn published_act_plans_workflows_with_typed_legacy_and_cohort_routes() {
    let binary = std::env::var("BOSN_ACT_RETENTION_TEST_BINARY").unwrap();
    let policy: CachePolicy = toml::from_str("repository_max_bytes=100\naggregate_max_bytes=200\nmax_age_secs=3600\nunused_age_secs=1800\nmaintenance_interval_secs=60\n").unwrap();
    let namespace = Namespace::parse("0123456789abcdef").unwrap();
    let routes = [
        CacheRoute::Legacy(namespace.clone()),
        CacheRoute::Cohort { namespace, policy },
    ];
    let invocations: Vec<Vec<String>> = routes
        .into_iter()
        .map(|cache_route| {
            let mut args = ActInvocation {
                event: "push".into(),
                workflow: format!("{ENGINE_WORK}/workflow.yaml"),
                workflow_overlaid: false,
                job: None,
                cache_policy: Default::default(),
                auto_retention: true,
                cache_route,
                secrets: Default::default(),
                params: Default::default(),
            }
            .args();
            args.push("-l".into());
            args
        })
        .collect();
    let result = std::process::Command::new("python3").args([
        "-c", r#"
import json, pathlib, subprocess, sys, tempfile
binary, invocations, cache, work = sys.argv[1:]
with tempfile.TemporaryDirectory() as directory:
    root = pathlib.Path(directory)
    private = root / 'work'
    (private / 'overlay').mkdir(parents=True)
    (private / 'event.json').write_text('{}')
    (private / 'workflow.yaml').write_text('on: [push]\njobs:\n  proof-job:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo proof\n')
    for index, invocation in enumerate(json.loads(invocations)):
        args = [value.replace(cache, str(root / 'cache')).replace(work, str(private)) for value in invocation]
        assert args.count('--cache-server-path') == 1, args
        path = args[args.index('--cache-server-path') + 1]
        assert ('cohort-v1' in path) == (index == 1), args
        assert args.count('--cache-server-cohort-root') == index, args
        command = subprocess.run([binary, *args], cwd=private, capture_output=True, text=True, timeout=15)
        assert command.returncode == 0, (command.stdout, command.stderr)
        assert 'proof-job' in command.stdout, (command.stdout, command.stderr)
"#, &binary, &serde_json::to_string(&invocations).unwrap(), ENGINE_CACHE, ENGINE_WORK,
    ]).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
#[ignore = "requires BOSN_ACT_RETENTION_TEST_BINARY pointing to verified pinned act2"]
fn opted_out_workflow_does_not_open_or_initialize_archive_storage() {
    let namespace = Namespace::parse("0123456789abcdef").unwrap();
    let invocation = ActInvocation {
        event: "push".into(),
        workflow: "workflow.yaml".into(),
        workflow_overlaid: false,
        job: None,
        cache_route: CacheRoute::Cohort {
            namespace,
            policy: CachePolicy::default(),
        },
        cache_policy: Default::default(),
        auto_retention: false,
        secrets: Default::default(),
        params: Default::default(),
    };
    let result = std::process::Command::new("python3")
        .args([
            "-c",
            r#"
import json, pathlib, subprocess, sys, tempfile
binary, encoded, cache, work = sys.argv[1:]
with tempfile.TemporaryDirectory() as temporary:
    root = pathlib.Path(temporary)
    private = root / 'work'
    (private / 'overlay').mkdir(parents=True)
    (private / 'event.json').write_text('{}')
    (private / 'workflow.yaml').write_text('on: [push]\njobs:\n  proof-job:\n    if: false\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n')
    args = [value.replace(cache, str(root / 'cache')).replace(work, str(private))
            for value in json.loads(encoded)]
    args.extend(['-n', '--artifact-server-addr', '127.0.0.1', '--cache-server-addr', '127.0.0.1'])
    archive = pathlib.Path(args[args.index('--cache-server-path') + 1])
    for existing in [False, True]:
        if existing:
            archive.mkdir(parents=True)
            (archive / 'preserved').write_bytes(b'archive evidence')
        completed = subprocess.run([binary, *args], cwd=private, stdin=subprocess.DEVNULL,
                                   capture_output=True, text=True, timeout=15)
        assert completed.returncode == 0, (completed.stdout, completed.stderr)
        if existing:
            assert sorted(item.name for item in archive.iterdir()) == ['preserved']
            assert (archive / 'preserved').read_bytes() == b'archive evidence'
        else:
            assert not archive.exists(), 'opted-out run initialized archive storage'
"#,
            &std::env::var("BOSN_ACT_RETENTION_TEST_BINARY").unwrap(),
            &serde_json::to_string(&invocation.args()).unwrap(),
            ENGINE_CACHE,
            ENGINE_WORK,
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
