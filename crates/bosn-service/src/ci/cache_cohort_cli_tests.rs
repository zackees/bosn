//! Opt-in command contract test using an actual act2 retention binary.
use super::cache_cohort::Namespace;
use super::cache_policy::CachePolicy;

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
