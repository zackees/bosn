//! `bosn secret`: provision daemon-owned task secrets (#308).
//!
//! Values are read from stdin (or once from `gh auth token`) and written to
//! `STATE_DIR/secrets/<name>` with mode 0600. No subcommand ever prints a value.

use std::{
    ffi::OsString,
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
};

use bosn_service::secrets::{self, SecretState};
use serde_json::json;

pub fn run(mut arguments: impl Iterator<Item = OsString>) {
    let verb = arguments
        .next()
        .unwrap_or_else(|| fail("expected set, status, or remove"));
    match verb.to_string_lossy().as_ref() {
        "set" => set(arguments),
        "status" | "list" => status(arguments),
        "remove" => remove(arguments),
        _ => fail("expected set, status, or remove"),
    }
}

fn state_dir_flag(
    arguments: &mut impl Iterator<Item = OsString>,
    slot: &mut Option<PathBuf>,
) -> Result<(), ()> {
    if slot.is_some() {
        return Err(());
    }
    let value = arguments.next().ok_or(())?;
    if value.is_empty() {
        return Err(());
    }
    *slot = Some(PathBuf::from(value));
    Ok(())
}

fn set(mut arguments: impl Iterator<Item = OsString>) {
    let mut name = None;
    let mut state_dir = None;
    let mut from_gh = false;
    while let Some(argument) = arguments.next() {
        let ok = match argument.to_string_lossy().as_ref() {
            "--state-dir" => state_dir_flag(&mut arguments, &mut state_dir),
            "--from-gh" if !from_gh => {
                from_gh = true;
                Ok(())
            }
            value if name.is_none() && !value.starts_with('-') => {
                name = Some(value.to_owned());
                Ok(())
            }
            _ => Err(()),
        };
        ok.unwrap_or_else(|()| usage());
    }
    let name = name.unwrap_or_else(|| usage());
    let state_dir = state_dir.unwrap_or_else(bosn_service::mcp::default_state_dir);
    let value = if from_gh {
        eprintln!(
            "bosn secret: warning: `gh auth token` is usually a long-lived OAuth token with repo/workflow scopes; every workflow step run locally can act with it. Prefer a fine-grained token with no scopes."
        );
        let output = Command::new("gh")
            .args(["auth", "token"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .unwrap_or_else(|_| fail("`gh` is unavailable"));
        if !output.status.success() {
            fail("`gh auth token` failed; run `gh auth login` first");
        }
        output.stdout
    } else {
        let mut buffer = Vec::new();
        std::io::stdin()
            .take(64 * 1024)
            .read_to_end(&mut buffer)
            .unwrap_or_else(|_| fail("stdin cannot be read"));
        buffer
    };
    secrets::write_secret(&state_dir, &name, &value).unwrap_or_else(|error| fail(&error));
    println!(
        "stored secret {name} in {}",
        secrets::secrets_dir(&state_dir).display()
    );
}

fn status(mut arguments: impl Iterator<Item = OsString>) {
    let mut state_dir = None;
    let mut json_output = false;
    while let Some(argument) = arguments.next() {
        let ok = match argument.to_string_lossy().as_ref() {
            "--state-dir" => state_dir_flag(&mut arguments, &mut state_dir),
            "--json" if !json_output => {
                json_output = true;
                Ok(())
            }
            _ => Err(()),
        };
        ok.unwrap_or_else(|()| usage());
    }
    let state_dir = state_dir.unwrap_or_else(bosn_service::mcp::default_state_dir);
    let rows = secrets::secret_status(&state_dir);
    if json_output {
        let rows: Vec<_> = rows
            .iter()
            .map(|(name, state)| {
                let (state, reason) = match state {
                    SecretState::Present => ("present", None),
                    SecretState::Missing => ("missing", None),
                    SecretState::Refused(reason) => ("refused", Some(reason.clone())),
                };
                json!({"name": name, "env": secrets::secret_env_name(name), "state": state, "reason": reason})
            })
            .collect();
        println!("{}", json!({ "secrets": rows }));
        return;
    }
    for (name, state) in rows {
        match state {
            SecretState::Present => println!("{name}\tpresent"),
            SecretState::Missing => println!("{name}\tmissing"),
            SecretState::Refused(reason) => println!("{name}\trefused: {reason}"),
        }
    }
}

fn remove(mut arguments: impl Iterator<Item = OsString>) {
    let mut name = None;
    let mut state_dir = None;
    while let Some(argument) = arguments.next() {
        let ok = match argument.to_string_lossy().as_ref() {
            "--state-dir" => state_dir_flag(&mut arguments, &mut state_dir),
            value if name.is_none() && !value.starts_with('-') => {
                name = Some(value.to_owned());
                Ok(())
            }
            _ => Err(()),
        };
        ok.unwrap_or_else(|()| usage());
    }
    let name = name.unwrap_or_else(|| usage());
    let state_dir = state_dir.unwrap_or_else(bosn_service::mcp::default_state_dir);
    secrets::remove_secret(&state_dir, &name).unwrap_or_else(|error| fail(&error));
    println!("removed secret {name}");
}

fn usage() -> ! {
    fail(
        "usage: bosn secret set NAME [--from-gh] [--state-dir STATE_DIR] (value on stdin) | status [--state-dir STATE_DIR] [--json] | remove NAME [--state-dir STATE_DIR]",
    )
}

fn fail(message: &str) -> ! {
    eprintln!("bosn secret: {message}");
    std::process::exit(2)
}
