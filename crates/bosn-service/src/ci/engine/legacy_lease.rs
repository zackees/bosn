//! A lifetime lease for all participating legacy cache servers.
use super::{ENGINE_CACHE, ENGINE_WORK};

pub(super) fn command(namespace: Option<&crate::ci::cache_cohort::Namespace>) -> Vec<String> {
    let mut acquire = "flock -s 8 || exit $?".to_string();
    if let Some(namespace) = namespace {
        let record = namespace.routing_record_path();
        // Any publication, even malformed or dangling, invalidates a stale
        // Legacy plan. Route parsing and enrollment remain planner duties.
        acquire.push_str(&format!(
            "; record={record}; directory=${{record%/*}}; \
             if [ -e \"$directory\" ] && {{ [ ! -d \"$directory\" ] || [ ! -r \"$directory\" ] || [ ! -x \"$directory\" ]; }}; then \
             echo 'shared cache routing directory is unavailable' >&2; exit 78; fi; \
             if [ -L \"$directory\" ] || [ -e \"$record\" ] || [ -L \"$record\" ]; then \
             echo 'legacy cache route changed; replan with shared routing evidence' >&2; exit 78; fi"
        ));
    }
    locked_command(&acquire)
}

/// Caller must separately exclude nonparticipating legacy peers.
/// Contention refuses immediately instead of waiting behind a live workflow.
pub(super) fn migration_command() -> Vec<String> {
    locked_command(
        "flock -x -n 8 || { echo 'participating legacy cache session is busy' >&2; exit 75; }",
    )
}

fn locked_command(acquire: &str) -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        format!(
            "mkdir -p {ENGINE_CACHE}/actcache || exit $?; \
             exec 8>>{ENGINE_CACHE}/actcache/.legacy-migration.lock || exit $?; \
             {acquire}; exec {ENGINE_WORK}/bin/act \"$@\""
        ),
        "bosn-act-cache-lease".into(),
    ]
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn stale_legacy_plan_waits_for_migration_then_refuses_published_route() {
        let namespace = crate::ci::cache_cohort::Namespace::parse("0123456789abcdef").unwrap();
        let script = r#"
import fcntl, json, os, pathlib, subprocess, sys, tempfile, time
with tempfile.TemporaryDirectory() as directory:
    cache = pathlib.Path(directory) / 'cache'
    work = pathlib.Path(directory) / 'work'
    control = cache / 'actcache'
    control.mkdir(parents=True)
    binary = work / 'bin' / 'act'
    binary.parent.mkdir(parents=True)
    ready = pathlib.Path(directory) / 'executed'
    binary.write_text('#!/bin/sh\nprintf started >"$BOSN_ROUTE_EXECUTED"\n')
    binary.chmod(0o700)
    attempted = pathlib.Path(directory) / 'attempted'
    command = [value.replace('flock -s 8', 'printf attempted >\"$BOSN_ROUTE_ATTEMPTED\"; flock -s 8').replace(sys.argv[2], str(cache)).replace(sys.argv[3], str(work))
               for value in json.loads(sys.argv[1])]
    route = pathlib.Path(sys.argv[4].replace(sys.argv[2], str(cache)))
    route.parent.mkdir()
    with (control / '.legacy-migration.lock').open('a') as migration:
        fcntl.flock(migration, fcntl.LOCK_EX)
        child = subprocess.Popen([*command, 'push'], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 env={**os.environ, 'BOSN_ROUTE_EXECUTED': str(ready), 'BOSN_ROUTE_ATTEMPTED': str(attempted)})
        try:
            deadline = time.monotonic() + 5
            while not attempted.exists():
                assert child.poll() is None and time.monotonic() < deadline
                time.sleep(0.01)
            assert child.poll() is None and not ready.exists(), 'writer crossed migration lease'
            route.write_text('published route, not parsed by stale invocation')
            fcntl.flock(migration, fcntl.LOCK_UN)
            _, error = child.communicate(timeout=5)
            assert child.returncode == 78 and b'replan' in error, (child.returncode, error)
            assert not ready.exists(), 'stale legacy binary executed after publication'
        finally:
            if child.poll() is None:
                child.kill(); child.wait(timeout=5)
    old_command = [value.replace(sys.argv[2], str(cache)).replace(sys.argv[3], str(work))
                   for value in json.loads(sys.argv[5])]
    old = subprocess.run(old_command, env={**os.environ, 'BOSN_ROUTE_EXECUTED': str(ready)}, capture_output=True, timeout=5)
    assert old.returncode == 0 and ready.exists(), 'old wrapper did not reproduce stale route admission'
    ready.unlink()
    for kind in ['malformed', 'dangling']:
        route.unlink()
        if kind == 'malformed': route.write_text('invalid')
        else: route.symlink_to(route.parent / 'missing')
        result = subprocess.run(command, env={**os.environ, 'BOSN_ROUTE_EXECUTED': str(ready), 'BOSN_ROUTE_ATTEMPTED': str(attempted)}, capture_output=True, timeout=5)
        assert result.returncode == 78 and not ready.exists()
    route.unlink()
    result = subprocess.run(command, env={**os.environ, 'BOSN_ROUTE_EXECUTED': str(ready), 'BOSN_ROUTE_ATTEMPTED': str(attempted)}, capture_output=True, timeout=5)
    assert result.returncode == 0 and ready.read_text() == 'started'
    ready.unlink()
    route.parent.rmdir()
    route.parent.write_text('not a routing directory')
    result = subprocess.run(command, env={**os.environ, 'BOSN_ROUTE_EXECUTED': str(ready), 'BOSN_ROUTE_ATTEMPTED': str(attempted)}, capture_output=True, timeout=5)
    assert result.returncode == 78 and not ready.exists()
"#;
        let result = std::process::Command::new("python3")
            .args([
                "-c",
                script,
                &serde_json::to_string(&command(Some(&namespace))).unwrap(),
                ENGINE_CACHE,
                ENGINE_WORK,
                &namespace.routing_record_path(),
                &serde_json::to_string(&command(None)).unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    const SCRIPT: &str = r#"
import fcntl, json, os, pathlib, subprocess, sys, tempfile, time
with tempfile.TemporaryDirectory() as directory:
    cache = pathlib.Path(directory) / 'cache'
    work = pathlib.Path(directory) / 'work'
    binary = work / 'bin' / 'act'
    binary.parent.mkdir(parents=True)
    binary.write_text('#!/usr/bin/env python3\nimport json,os,pathlib,sys,time\nos.fstat(8)\npathlib.Path(os.environ["BOSN_LEASE_READY"]).write_text(json.dumps(sys.argv[1:]))\ntime.sleep(60)\n')
    binary.chmod(0o700)
    command = [value.replace(sys.argv[3], str(cache)).replace(sys.argv[4], str(work))
               for value in json.loads(sys.argv[1])]
    migration = [value.replace(sys.argv[3], str(cache)).replace(sys.argv[4], str(work))
                 for value in json.loads(sys.argv[2])]
    children = []
    try:
        expected = ['push', '--env', 'literal with spaces; $(false)']
        for index in range(2):
            ready = pathlib.Path(directory) / ('ready-' + str(index))
            child = subprocess.Popen([*command, *expected],
                                     env={**os.environ, 'BOSN_LEASE_READY': str(ready)})
            children.append(child)
            deadline = time.monotonic() + 5
            while not ready.exists():
                assert child.poll() is None, child.returncode
                assert time.monotonic() < deadline, 'child never acquired shared lease'
                time.sleep(0.01)
            assert json.loads(ready.read_text()) == expected
        with (cache / 'actcache' / '.legacy-migration.lock').open('rb') as observer:
            def blocked():
                try:
                    fcntl.flock(observer, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    return
                raise AssertionError('migration lease acquired while a server lives')
            blocked()
            refused = subprocess.run([*migration, 'cache', 'import'],
                                      env={**os.environ, 'BOSN_LEASE_READY': str(cache / 'refused')},
                                      capture_output=True, text=True, timeout=5)
            assert refused.returncode == 75, (refused.returncode, refused.stderr)
            assert 'busy' in refused.stderr and not (cache / 'refused').exists()
            children[0].kill(); children[0].wait(timeout=5)
            blocked()
            children[1].kill(); children[1].wait(timeout=5)
            fcntl.flock(observer, fcntl.LOCK_EX | fcntl.LOCK_NB)
            fcntl.flock(observer, fcntl.LOCK_UN)
            ready = cache / 'migration-ready'
            child = subprocess.Popen([*migration, 'cache', 'import'],
                                     env={**os.environ, 'BOSN_LEASE_READY': str(ready)})
            children.append(child)
            deadline = time.monotonic() + 5
            while not ready.exists():
                assert child.poll() is None, child.returncode
                assert time.monotonic() < deadline, 'migration never acquired exclusive lease'
                time.sleep(0.01)
            try:
                fcntl.flock(observer, fcntl.LOCK_SH | fcntl.LOCK_NB)
            except BlockingIOError:
                pass
            else:
                raise AssertionError('legacy reader acquired lease during migration')
            child.kill(); child.wait(timeout=5)
            fcntl.flock(observer, fcntl.LOCK_SH | fcntl.LOCK_NB)
    finally:
        for child in children:
            if child.poll() is None:
                child.kill(); child.wait(timeout=5)
"#;
    #[test]
    fn shared_lease_spans_exec_and_releases_after_last_server_dies() {
        let result = std::process::Command::new("python3")
            .args([
                "-c",
                SCRIPT,
                &serde_json::to_string(&command(None)).unwrap(),
                &serde_json::to_string(&migration_command()).unwrap(),
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
}
