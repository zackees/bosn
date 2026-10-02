//! A step's last output line reaches the log even without a trailing
//! newline (#398).
//!
//! act 0.2.88 splits a step's output into log lines with
//! `common.NewLineWriter` (`pkg/common/line_writer.go`): `Write` hands a line
//! on only at `\n` and keeps the rest buffered, and the writer has no flush.
//! `useStepLogger` (`pkg/runner/job_executor.go`) installs a fresh writer for
//! each step and drops it when the step ends, so a final partial line, such as
//! `printf 'fatal: …' >&2; exit 1`, never reaches act's JSON log.
//!
//! bosn cannot reach act's buffer, so it ends the step's output itself: in the
//! run's copy of the workflows every POSIX-shell `run:` script starts with an
//! EXIT trap that writes [`MARK`] and a newline to stderr. stdout and stderr
//! share the step's one line writer, so that newline completes any partial
//! line, whichever stream it was on. The parser takes the mark off again
//! ([`unmark`]) and drops a line that held nothing else. A script that sets
//! its own EXIT trap replaces bosn's and keeps act's behaviour.
//!
//! act names an unnamed `run:` step after its script, so the trap would show
//! wherever such a step is named; [`untrap`] takes it off again (#405).

use std::borrow::Cow;

use serde::Deserialize;
use serde_yaml::Value;

/// The mark's text after its leading ASCII record separator (`\x1e`).
macro_rules! tag {
    () => {
        "bosn:eol"
    };
}

/// What the trap writes before its newline: unlikely in real output, and not
/// `::`-prefixed, so act never reads a bare mark as a workflow command.
pub const MARK: &str = concat!("\u{1e}", tag!());

/// Prefixed to the script on its first line, so its line numbers stay put.
/// `|| :` keeps a failed write (stderr closed) from touching the exit status.
const TRAP: &str = concat!("trap 'printf \"\\036", tag!(), "\\n\" >&2 || :' EXIT; ");

/// A raw output line with bosn's mark taken off its end; `None` when the mark
/// was all the line held.
pub fn unmark(line: &str) -> Option<&str> {
    match line.strip_suffix(MARK) {
        Some("") => None,
        Some(rest) => Some(rest),
        None => Some(line),
    }
}

/// `text` without bosn's trap: a step's name (act's `step` field, or a
/// declared step's `run:` text) or an act notice that names the step
/// (`⭐ Run Main …`, `✅  Success - Main …`), as the workflow wrote it.
pub fn untrap(text: &str) -> Cow<'_, str> {
    if text.contains(TRAP) {
        Cow::Owned(text.replace(TRAP, ""))
    } else {
        Cow::Borrowed(text)
    }
}

/// `defaults:` of a workflow or job; only the run shell matters here.
#[derive(Default, Deserialize)]
struct Defaults {
    #[serde(default)]
    run: RunDefaults,
}

#[derive(Default, Deserialize)]
struct RunDefaults {
    shell: Option<String>,
}

impl Defaults {
    fn of(owner: &Value) -> Self {
        owner
            .get("defaults")
            .and_then(|d| serde_yaml::from_value(d.clone()).ok())
            .unwrap_or_default()
    }
}

/// The fields of a `run:` step that decide whether it gets the trap.
#[derive(Deserialize)]
struct RunStep {
    run: String,
    shell: Option<String>,
}

/// Add the trap to every POSIX-shell `run:` step of a workflow (under
/// `jobs.<id>.steps`, after `defaults.run.shell` at job then workflow level,
/// as act resolves it) or of a composite action (`runs.steps`). Returns how
/// many steps changed.
pub fn add_traps(document: &mut Value) -> usize {
    let workflow = Defaults::of(document).run.shell;
    let mut changed = 0;
    if let Some(jobs) = document.get_mut("jobs").and_then(Value::as_mapping_mut) {
        for (_, job) in jobs.iter_mut() {
            let shell = Defaults::of(job).run.shell.or_else(|| workflow.clone());
            changed += trap_steps(job.get_mut("steps"), shell.as_deref());
        }
    }
    if let Some(runs) = document.get_mut("runs") {
        changed += trap_steps(runs.get_mut("steps"), None);
    }
    changed
}

fn trap_steps(steps: Option<&mut Value>, default_shell: Option<&str>) -> usize {
    let Some(steps) = steps.and_then(Value::as_sequence_mut) else {
        return 0;
    };
    steps
        .iter_mut()
        .map(|step| usize::from(trap_step(step, default_shell)))
        .sum()
}

/// Prefix one step's script; `false` when it is not a POSIX-shell `run:`
/// step or already has the trap.
fn trap_step(step: &mut Value, default_shell: Option<&str>) -> bool {
    let Ok(parsed) = serde_yaml::from_value::<RunStep>(step.clone()) else {
        return false;
    };
    let shell = parsed.shell.as_deref().filter(|s| !s.trim().is_empty());
    if !is_posix(shell.or(default_shell)) || parsed.run.starts_with(TRAP) {
        return false;
    }
    let Some(step) = step.as_mapping_mut() else {
        return false;
    };
    step.insert("run".into(), format!("{TRAP}{}", parsed.run).into());
    true
}

/// act's default (no shell anywhere) is `bash`, or `sh` where bash is
/// missing; a named shell counts when its program is `bash` or `sh`. An
/// expression is resolved only by act, so it is left alone.
fn is_posix(shell: Option<&str>) -> bool {
    let Some(shell) = shell.map(str::trim).filter(|s| !s.is_empty()) else {
        return true;
    };
    if shell.contains("${{") {
        return false;
    }
    let program = shell.split_whitespace().next().unwrap_or_default();
    matches!(program.rsplit('/').next(), Some("bash" | "sh"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn traps(yaml: &str) -> (usize, Value) {
        let mut document: Value = serde_yaml::from_str(yaml).unwrap();
        (add_traps(&mut document), document)
    }

    fn run<'a>(document: &'a Value, job: &str, step: usize) -> &'a str {
        document["jobs"][job]["steps"][step]["run"]
            .as_str()
            .unwrap()
    }

    #[test]
    fn posix_run_steps_get_the_trap_on_their_first_line() {
        let (changed, document) = traps(
            "on: push\njobs:\n  a:\n    steps:\n      - run: \"printf 'fatal: x' >&2; exit 1\"\n      - shell: sh\n        run: |\n          echo one\n          printf two\n      - shell: bash --noprofile --norc -eo pipefail {0}\n        run: 'true'\n      - uses: actions/setup-node@v4\n      - shell: python\n        run: print(1)\n      - shell: pwsh\n        run: Write-Host 1\n      - shell: ${{ matrix.shell }}\n        run: 'true'\n",
        );
        assert_eq!(changed, 3);
        assert_eq!(
            run(&document, "a", 0),
            format!("{TRAP}printf 'fatal: x' >&2; exit 1")
        );
        assert_eq!(
            run(&document, "a", 1),
            format!("{TRAP}echo one\nprintf two\n")
        );
        assert!(run(&document, "a", 2).starts_with(TRAP));
        for step in 4..=6 {
            assert!(!run(&document, "a", step).contains("trap"), "step {step}");
        }
    }

    #[test]
    fn defaults_decide_the_shell_of_a_step_that_names_none() {
        let (changed, document) = traps(
            "defaults:\n  run:\n    shell: pwsh\njobs:\n  win:\n    steps:\n      - run: Write-Host 1\n      - shell: bash\n        run: 'true'\n  lin:\n    defaults:\n      run:\n        shell: bash\n    steps:\n      - run: 'true'\n",
        );
        assert_eq!(changed, 2);
        assert_eq!(run(&document, "win", 0), "Write-Host 1");
        assert!(run(&document, "win", 1).starts_with(TRAP));
        assert!(run(&document, "lin", 0).starts_with(TRAP));
    }

    #[test]
    fn composite_actions_are_trapped_and_a_second_pass_changes_nothing() {
        let yaml =
            "runs:\n  using: composite\n  steps:\n    - shell: bash\n      run: printf partial\n";
        let (changed, mut document) = traps(yaml);
        assert_eq!(changed, 1);
        assert_eq!(add_traps(&mut document), 0, "idempotent");
    }

    #[test]
    fn the_parser_takes_the_mark_off_and_drops_a_line_of_only_the_mark() {
        assert_eq!(
            unmark(&format!("fatal: no newline{MARK}")),
            Some("fatal: no newline")
        );
        assert_eq!(unmark(MARK), None);
        assert_eq!(unmark("plain"), Some("plain"));
        assert_eq!(unmark(""), Some(""), "a real blank line stays");
    }

    /// The trap and the mark are written once each; prove the shell's
    /// output is exactly a completed partial line ending in the mark, with
    /// the script's exit status kept.
    #[test]
    #[cfg(unix)]
    fn the_trap_completes_a_partial_line_and_keeps_the_exit_status() {
        let script = format!("{TRAP}printf 'fatal: no newline' >&2; exit 3");
        let output = std::process::Command::new("sh")
            .args(["-e", "-c", &script])
            .output()
            .expect("sh runs");
        assert_eq!(output.status.code(), Some(3));
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            format!("fatal: no newline{MARK}\n")
        );
    }
}
