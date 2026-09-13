//! Synthetic Docker CLI used only by resolver integration tests.

use std::{env, fs, io::Write as _};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let log = env::var("BOSN_RESOLVER_LOG").expect("test log path");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .expect("open test log");
    writeln!(file, "{}", args.join("\u{1f}")).expect("write test log");

    let scenario = env::var("BOSN_RESOLVER_SCENARIO").expect("test scenario");
    let command = &args[1..]; // Strip `--resolver-helper`.
    let is_inspect = command.starts_with(&["image".into(), "inspect".into()]);
    let is_pull = command.first().is_some_and(|arg| arg == "pull");
    let inspect_count = fs::read_to_string(log)
        .expect("read test log")
        .lines()
        .filter(|line| line.contains("\u{1f}image\u{1f}inspect\u{1f}"))
        .count();

    match scenario.as_str() {
        "present" if is_inspect => println!("sha256:{}", "a".repeat(64)),
        "automatic-platform" if command.starts_with(&["version".into()]) => println!("linux/amd64"),
        "automatic-platform-present" if command.starts_with(&["version".into()]) => {
            println!("linux/amd64")
        }
        "automatic-platform-present" if is_inspect => println!("sha256:{}", "c".repeat(64)),
        "slow-present" if is_inspect => {
            std::thread::sleep(std::time::Duration::from_millis(500));
            println!("sha256:{}", "a".repeat(64));
        }
        "malformed" if is_inspect => println!("not-an-image-id"),
        "missing" if is_inspect => {
            eprintln!("Error response from daemon: No such image: alpine");
            std::process::exit(1);
        }
        "missing-then-present" if is_inspect && inspect_count == 1 => {
            eprintln!("Error response from daemon: No such image: alpine");
            std::process::exit(1);
        }
        "missing-then-present" if is_pull => {}
        "missing-then-present" if is_inspect => println!("sha256:{}", "b".repeat(64)),
        "missing-then-malformed" if is_inspect && inspect_count == 1 => {
            eprintln!("Error response from daemon: No such image: alpine");
            std::process::exit(1);
        }
        "missing-then-malformed" if is_pull => {}
        "missing-then-malformed" if is_inspect => println!("not-an-image-id"),
        "permission" if is_inspect => {
            eprintln!("permission denied while trying to connect to Docker daemon");
            std::process::exit(1);
        }
        "unsupported" if is_inspect => {
            eprintln!("unknown flag: --platform");
            std::process::exit(1);
        }
        "missing-then-permission" if is_inspect && inspect_count == 1 => {
            eprintln!("Error response from daemon: No such image: alpine");
            std::process::exit(1);
        }
        "missing-then-permission" if is_pull => {}
        "missing-then-permission" if is_inspect => {
            eprintln!("permission denied while trying to connect to Docker daemon");
            std::process::exit(1);
        }
        "near-missing" if is_inspect => {
            eprintln!("Error response from daemon: No such image: alpine-other");
            std::process::exit(1);
        }
        _ => panic!("unexpected helper command {scenario:?}: {args:?}"),
    }
}
