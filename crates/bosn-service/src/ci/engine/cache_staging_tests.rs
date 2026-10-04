//! Execute the production shell with a deterministic Docker transport.
use super::*;
use std::{
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
};

const DOCKER: &str = r#"#!/usr/bin/env python3
import os, sys, time
from pathlib import Path
args = sys.argv[1:]
def record(kind):
    with open(os.environ['EVENTS'], 'a') as log:
        log.write(f'{kind} {os.getpid()} {time.monotonic_ns()}\n')
if args[0] == 'pull':
    record('pull')
    time.sleep(.2)
elif args[0] == 'save':
    record('save')
    Path(args[args.index('-o') + 1]).write_text('verified-archive')
    if os.environ.get('SAVE_FAIL') == '1':
        sys.exit(2)
elif args[0] == 'load':
    archive = Path(args[args.index('-i') + 1])
    if archive.read_text() != 'verified-archive':
        sys.exit(1)
    record('start')
    if os.environ.get('WARM_PAIR') == '1':
        deadline = time.monotonic() + 5
        while sum(line.startswith('start ') for line in Path(os.environ['EVENTS']).read_text().splitlines()) < 2:
            if time.monotonic() >= deadline:
                sys.exit(3)
            time.sleep(.01)
    record('end')
elif args[0] not in ('tag', 'image'):
    sys.exit(2)
"#;

#[test]
fn cold_concurrent_restores_publish_once_and_warm_readers_overlap() {
    let dir = tempfile::tempdir().unwrap();
    let mock = dir.path().join("docker");
    std::fs::write(&mock, DOCKER).unwrap();
    std::fs::set_permissions(&mock, std::fs::Permissions::from_mode(0o755)).unwrap();
    let events = dir.path().join("events");
    let script = load_runner_script().replace(ENGINE_CACHE, dir.path().to_str().unwrap());
    let pair = |warm| {
        let spawn = || {
            Command::new("sh")
                .args(["-ec", &script])
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        dir.path().display(),
                        std::env::var("PATH").unwrap()
                    ),
                )
                .env("EVENTS", &events)
                .env("WARM_PAIR", if warm { "1" } else { "0" })
                .stdout(Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut first = spawn();
        let mut second = spawn();
        assert!(first.wait().unwrap().success());
        assert!(second.wait().unwrap().success());
    };
    pair(false);
    let cold = std::fs::read_to_string(&events).unwrap();
    assert_eq!(
        cold.lines()
            .filter(|line| line.starts_with("pull "))
            .count(),
        1
    );
    assert_eq!(
        cold.lines()
            .filter(|line| line.starts_with("save "))
            .count(),
        1
    );
    std::fs::write(&events, "").unwrap();
    pair(true);
    let warm = std::fs::read_to_string(&events).unwrap();
    let times = |kind| {
        warm.lines()
            .filter_map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                (fields[0] == kind).then(|| fields[2].parse::<u64>().unwrap())
            })
            .collect::<Vec<_>>()
    };
    let starts = times("start");
    let ends = times("end");
    assert_eq!((starts.len(), ends.len()), (2, 2));
    assert!(
        starts.iter().max().unwrap() < ends.iter().min().unwrap(),
        "warm loads serialized: {warm}"
    );
    assert!(!warm.contains("pull ") && !warm.contains("save "));
    let archive = dir
        .path()
        .join("images")
        .join(format!("{}.tar", runner_tag().replace([':', '/'], "-")));
    std::fs::remove_file(&archive).unwrap();
    let failed = Command::new("sh")
        .args(["-ec", &script])
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("EVENTS", &events)
        .env("SAVE_FAIL", "1")
        .status()
        .unwrap();
    assert!(!failed.success(), "failed publication must be reported");
    assert!(!archive.exists());
    let files: Vec<_> = std::fs::read_dir(archive.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(files.len(), 1, "failed stage survived: {files:?}");
    assert!(files[0].to_string_lossy().ends_with(".lock"));
    pair(false);
    assert!(archive.exists(), "lock was not released after failed save");
}
