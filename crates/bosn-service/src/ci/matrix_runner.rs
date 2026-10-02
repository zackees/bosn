//! A matrix leg runs on its own runner or is reported unsupported, never on a
//! sibling's (#404).
//!
//! act 0.2.88 resolves `runs-on` in `RunContext.runsOnPlatformNames`
//! (`pkg/runner/run_context.go`) by evaluating the job's `runs-on` node *in
//! place*, and every leg of a matrix shares that node. The first leg to
//! evaluate `${{ matrix.os }}` writes its own value over the expression, so
//! the others see it instead of theirs, in whatever order the legs' goroutines
//! reach it: one run marks the Linux leg unsupported, the next runs the
//! Windows leg on Linux and calls it a pass.
//!
//! bosn takes that decision away from act. In the run's copy of the workflow,
//! a job whose `runs-on` is one matrix expression ([`gate`]):
//! - runs on `ubuntu-latest`, a constant, so there is nothing to race on;
//! - runs each of its steps only when the leg's own runner (the original
//!   expression, evaluated per leg by the step's `if:`, which act never
//!   writes back) is one bosn runs locally ([`LOCAL_RUNNER_LABELS`]);
//! - otherwise runs only a last step that prints [`MARK`] and that runner,
//!   which the parser reads as the leg being unsupported.

use serde::Deserialize;
use serde_yaml::{Mapping, Value};

use super::engine::LOCAL_RUNNER_LABELS;

/// What the gate step prints before the leg's runner: unlikely in real output
/// and not `::`-prefixed, so act never reads it as a workflow command.
pub const MARK: &str = "\u{1e}bosn:unsupported-runner";

/// The gate step's `id:`; its index-free id keeps the job's own step ids
/// (their indexes) as written, and [`super::workflow`] does not list it.
pub const STEP_ID: &str = "bosn-unsupported-runner";

/// A log line as shown: the gate's [`MARK`] line says what it means.
pub fn describe(line: &str) -> String {
    match line.strip_prefix(MARK) {
        Some(runner) => format!(
            "bosn: runs-on {} is not a local runner; this leg is unsupported",
            runner.trim()
        ),
        None => line.to_string(),
    }
}

/// The fields that decide whether a job is gated.
#[derive(Deserialize)]
struct Job {
    #[serde(rename = "runs-on")]
    runs_on: Option<Value>,
    container: Option<Value>,
    #[serde(default)]
    steps: Vec<Value>,
}

impl Job {
    /// The expression inside a `runs-on: ${{ … }}` that reads the matrix.
    /// A list, a literal, or a job with a `container:` (act then never reads
    /// `runs-on` to decide) is left to act.
    fn matrix_runner(&self) -> Option<String> {
        if self.container.is_some() || self.steps.is_empty() {
            return None;
        }
        let expression = unwrap_expression(self.runs_on.as_ref()?.as_str()?)?;
        expression
            .contains("matrix")
            .then(|| expression.to_string())
    }
}

/// `E` for `${{ E }}`; `None` for anything else.
fn unwrap_expression(text: &str) -> Option<&str> {
    let inner = text.trim().strip_prefix("${{")?.strip_suffix("}}")?.trim();
    (!inner.is_empty() && !inner.contains("${{")).then_some(inner)
}

/// Gate every job of a workflow document whose `runs-on` is a matrix
/// expression; returns the gated job IDs.
pub fn gate(document: &mut Value) -> Vec<String> {
    let Some(jobs) = document.get_mut("jobs").and_then(Value::as_mapping_mut) else {
        return Vec::new();
    };
    let mut gated = Vec::new();
    for (id, job) in jobs.iter_mut() {
        let Some(id) = id.as_str() else { continue };
        let Some(runner) = serde_yaml::from_value::<Job>(job.clone())
            .ok()
            .and_then(|parsed| parsed.matrix_runner())
        else {
            continue;
        };
        let Some(job) = job.as_mapping_mut() else {
            continue;
        };
        gate_job(job, &runner);
        gated.push(id.to_string());
    }
    gated
}

fn gate_job(job: &mut Mapping, runner: &str) {
    let labels = serde_json::to_string(&LOCAL_RUNNER_LABELS).unwrap_or_default();
    let local = format!("contains(fromJSON('{labels}'), {runner})");
    job.insert("runs-on".into(), LOCAL_RUNNER_LABELS[0].into());
    let Some(steps) = job.get_mut("steps").and_then(Value::as_sequence_mut) else {
        return;
    };
    for step in steps.iter_mut().filter_map(Value::as_mapping_mut) {
        let condition = match step.get("if").and_then(condition) {
            Some(own) => format!("${{{{ {local} && ({own}) }}}}"),
            None => format!("${{{{ {local} }}}}"),
        };
        step.insert("if".into(), condition.into());
    }
    let mut env = Mapping::new();
    env.insert("BOSN_RUNS_ON".into(), format!("${{{{ {runner} }}}}").into());
    let mut gate = Mapping::new();
    gate.insert("id".into(), STEP_ID.into());
    gate.insert("name".into(), "bosn: no local runner for this leg".into());
    gate.insert("if".into(), format!("${{{{ !{local} }}}}").into());
    gate.insert("shell".into(), "bash".into());
    gate.insert("env".into(), Value::Mapping(env));
    gate.insert(
        "run".into(),
        r#"printf '\036bosn:unsupported-runner %s\n' "$BOSN_RUNS_ON""#.into(),
    );
    steps.push(Value::Mapping(gate));
}

/// A step's own `if:` as an expression: a bool, or a string with or without
/// its `${{ }}`. Anything else (null, a number) is no condition.
fn condition(value: &Value) -> Option<String> {
    match value {
        Value::Bool(b) => Some(b.to_string()),
        Value::String(text) if !text.trim().is_empty() => {
            Some(unwrap_expression(text).unwrap_or(text.trim()).to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL: &str =
        r#"contains(fromJSON('["ubuntu-latest","ubuntu-24.04","ubuntu-22.04"]'), matrix.os)"#;

    #[test]
    fn a_matrix_runner_is_decided_per_leg_by_bosn_not_act() {
        let mut document: Value = serde_yaml::from_str(include_str!(
            "../../tests/fixtures/act/act-0.2.88-matrix-legs-runner.yml"
        ))
        .unwrap();
        let mut wheel = document["jobs"]["wheel"].clone();
        wheel["steps"][0]["if"] = Value::Bool(false);
        wheel["steps"]
            .as_sequence_mut()
            .unwrap()
            .push(serde_yaml::from_str("{if: '${{ always() }}', run: echo post}").unwrap());
        document["jobs"]["wheel2"] = wheel;
        assert_eq!(gate(&mut document), ["wheel", "wheel2"]);
        let wheel = &document["jobs"]["wheel"];
        assert_eq!(wheel["runs-on"], "ubuntu-latest", "nothing left to race on");
        assert_eq!(wheel["name"], "Wheel (${{ matrix.os }})");
        assert_eq!(wheel["steps"][0]["if"], format!("${{{{ {LOCAL} }}}}"));
        assert_eq!(wheel["steps"][0]["run"], "echo leg ${{ matrix.os }}");
        let gate_step = &wheel["steps"][1];
        assert_eq!(gate_step["id"], STEP_ID);
        assert_eq!(gate_step["if"], format!("${{{{ !{LOCAL} }}}}"));
        assert_eq!(gate_step["env"]["BOSN_RUNS_ON"], "${{ matrix.os }}");
        let steps = &document["jobs"]["wheel2"]["steps"];
        assert_eq!(steps[0]["if"], format!("${{{{ {LOCAL} && (false) }}}}"));
        assert_eq!(
            steps[1]["if"],
            format!("${{{{ {LOCAL} && (always()) }}}}"),
            "a step's own condition, status functions included, still holds"
        );
        for untouched in ["first", "lint"] {
            assert_eq!(document["jobs"][untouched]["runs-on"], "ubuntu-latest");
            assert!(document["jobs"][untouched]["steps"][0].get("if").is_none());
        }
    }

    #[test]
    fn literal_list_container_and_non_matrix_runners_are_left_to_act() {
        let mut document: Value = serde_yaml::from_str(
            "jobs:\n  a: {runs-on: windows-latest, steps: [{run: x}]}\n  b: {runs-on: [self-hosted, linux], steps: [{run: x}]}\n  c: {runs-on: '${{ matrix.os }}', container: alpine, steps: [{run: x}]}\n  d: {runs-on: '${{ inputs.runner }}', steps: [{run: x}]}\n  e: {runs-on: '${{ matrix.os }}', uses: ./.github/workflows/x.yml}\n",
        )
        .unwrap();
        let before = document.clone();
        assert!(gate(&mut document).is_empty());
        assert_eq!(document, before);
    }
}
