//! A lifetime lease for all participating legacy cache servers.
use super::{ENGINE_CACHE, ENGINE_WORK};

pub(super) fn command() -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        format!(
            "mkdir -p {ENGINE_CACHE}/actcache || exit $?; \
             exec 8>>{ENGINE_CACHE}/actcache/.legacy-migration.lock || exit $?; \
             flock -s 8 || exit $?; exec {ENGINE_WORK}/bin/act \"$@\""
        ),
        "bosn-act-cache-lease".into(),
    ]
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    const SCRIPT: &str = r#"
import fcntl, json, os, pathlib, subprocess, sys, tempfile, time
with tempfile.TemporaryDirectory() as directory:
    cache = pathlib.Path(directory) / 'cache'
    work = pathlib.Path(directory) / 'work'
    binary = work / 'bin' / 'act'
    binary.parent.mkdir(parents=True)
    binary.write_text('#!/usr/bin/env python3\nimport json,os,pathlib,sys,time\nos.fstat(8)\npathlib.Path(os.environ["BOSN_LEASE_READY"]).write_text(json.dumps(sys.argv[1:]))\ntime.sleep(60)\n')
    binary.chmod(0o700)
    command = [value.replace(sys.argv[2], str(cache)).replace(sys.argv[3], str(work))
               for value in json.loads(sys.argv[1])]
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
            children[0].kill(); children[0].wait(timeout=5)
            blocked()
            children[1].kill(); children[1].wait(timeout=5)
            fcntl.flock(observer, fcntl.LOCK_EX | fcntl.LOCK_NB)
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
                &serde_json::to_string(&command()).unwrap(),
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
