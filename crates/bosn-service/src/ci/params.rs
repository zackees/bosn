//! Workflow inputs, a matrix filter and extra environment for one run (#430).
//!
//! `bosn ci run --input K=V --matrix K:V --env K=V`, each repeatable. They are
//! validated here like the other plan fields (bounded sizes, safe keys),
//! recorded in the run record, folded into its coalescing key, and passed to
//! act as `--input`, `--matrix` and `--env`. Inputs also go into the event
//! payload (`inputs`), so `workflow_dispatch` and `workflow_call` see them.
//!
//! Secrets never travel through `--env`: a key that names a credential
//! (`*TOKEN*`, `*SECRET*`, `*PASSWORD*`, ...) or the runner's reserved
//! `GITHUB_*`/`ACTIONS_*`/`RUNNER_*` namespace is refused. The daemon-owned
//! `github_token` stays the only secret, passed as act `-s`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::provider::Trigger;

/// At most this many entries per kind.
pub const MAX_ENTRIES: usize = 32;
/// The longest key.
pub const MAX_KEY: usize = 100;
/// The longest value.
pub const MAX_VALUE: usize = 4096;

/// Credential-like words an `--env` key may not contain.
const SECRET_WORDS: [&str; 6] = [
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "PRIVATE_KEY",
];
/// Namespaces the runner owns.
const RESERVED_ENV_PREFIXES: [&str; 4] = ["GITHUB_", "ACTIONS_", "RUNNER_", "ACT_"];

/// One run's workflow inputs, matrix filter and extra environment.
#[derive(
    Clone,
    Debug,
    Default,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct RunParams {
    /// `inputs.<K>` for a `workflow_dispatch` or `workflow_call` run.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, String>,
    /// Run only the matrix legs whose `K` is `V` (act `--matrix K:V`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub matrix: BTreeMap<String, String>,
    /// Extra environment for every job (act `--env K=V`); never a secret.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

impl RunParams {
    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty() && self.matrix.is_empty() && self.env.is_empty()
    }

    /// Add one `--input K=V`.
    pub fn add_input(&mut self, spec: &str) -> Result<(), String> {
        let (key, value) = split(spec, '=', "--input", "K=V")?;
        insert(&mut self.inputs, "--input", key, value)
    }

    /// Add one `--matrix K:V`.
    pub fn add_matrix(&mut self, spec: &str) -> Result<(), String> {
        let (key, value) = split(spec, ':', "--matrix", "K:V")?;
        insert(&mut self.matrix, "--matrix", key, value)
    }

    /// Add one `--env K=V`.
    pub fn add_env(&mut self, spec: &str) -> Result<(), String> {
        let (key, value) = split(spec, '=', "--env", "K=V")?;
        insert(&mut self.env, "--env", key, value)
    }

    /// Bounded sizes, safe keys and values, no secret in the environment,
    /// and inputs only for an event that takes them.
    pub fn validate(&self, trigger: Trigger) -> Result<(), String> {
        for (what, map) in [
            ("--input", &self.inputs),
            ("--matrix", &self.matrix),
            ("--env", &self.env),
        ] {
            if map.len() > MAX_ENTRIES {
                return Err(format!("at most {MAX_ENTRIES} {what} entries"));
            }
            for (key, value) in map {
                check_key(what, key)?;
                check_value(what, key, value)?;
            }
        }
        for key in self.matrix.keys() {
            if self.matrix[key].contains(':') {
                return Err(format!("--matrix {key}: a value cannot contain ':'"));
            }
        }
        for key in self.env.keys() {
            if !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || key.as_bytes()[0].is_ascii_digit()
            {
                return Err(format!(
                    "--env {key}: an environment name is letters, digits and '_'"
                ));
            }
            let upper = key.to_ascii_uppercase();
            if SECRET_WORDS.iter().any(|word| upper.contains(word))
                || RESERVED_ENV_PREFIXES
                    .iter()
                    .any(|prefix| upper.starts_with(prefix))
            {
                return Err(format!(
                    "--env {key}: secrets and runner-owned names never travel through --env \
                     (use --github-token for the daemon-owned token)"
                ));
            }
        }
        if !self.inputs.is_empty() && !trigger.takes_inputs() {
            return Err(format!(
                "--input needs --event workflow_dispatch or workflow_call, not {}",
                trigger.as_str()
            ));
        }
        Ok(())
    }

    /// act's `--input`, `--matrix` and `--env` arguments, in key order.
    pub fn act_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        for (key, value) in &self.inputs {
            args.extend(["--input".into(), format!("{key}={value}")]);
        }
        for (key, value) in &self.matrix {
            args.extend(["--matrix".into(), format!("{key}:{value}")]);
        }
        for (key, value) in &self.env {
            args.extend(["--env".into(), format!("{key}={value}")]);
        }
        args
    }

    /// `inputs: k=v, ...; matrix: k:v; env: k=v`, or `None` when empty.
    pub fn describe(&self) -> Option<String> {
        let part = |name: &str, map: &BTreeMap<String, String>, sep: char| {
            (!map.is_empty()).then(|| {
                let entries: Vec<String> =
                    map.iter().map(|(k, v)| format!("{k}{sep}{v}")).collect();
                format!("{name}: {}", entries.join(", "))
            })
        };
        let parts: Vec<String> = [
            part("inputs", &self.inputs, '='),
            part("matrix", &self.matrix, ':'),
            part("env", &self.env, '='),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

fn split<'a>(
    spec: &'a str,
    separator: char,
    what: &str,
    shape: &str,
) -> Result<(&'a str, &'a str), String> {
    spec.split_once(separator)
        .filter(|(key, _)| !key.is_empty())
        .ok_or_else(|| format!("{what} takes {shape}, not {spec:?}"))
}

fn insert(
    map: &mut BTreeMap<String, String>,
    what: &str,
    key: &str,
    value: &str,
) -> Result<(), String> {
    if map.insert(key.into(), value.into()).is_some() {
        return Err(format!("{what} {key} given twice"));
    }
    Ok(())
}

fn check_key(what: &str, key: &str) -> Result<(), String> {
    let mut bytes = key.bytes();
    let first_ok = bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_');
    if !first_ok
        || key.len() > MAX_KEY
        || !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(format!(
            "{what} key {key:?} must start with a letter or '_' and hold only letters, \
             digits, '_' and '-' (at most {MAX_KEY})"
        ));
    }
    Ok(())
}

fn check_value(what: &str, key: &str, value: &str) -> Result<(), String> {
    if value.len() > MAX_VALUE || value.chars().any(char::is_control) {
        return Err(format!(
            "{what} {key}: a value is at most {MAX_VALUE} bytes with no control characters"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installer() -> RunParams {
        let mut params = RunParams::default();
        params.add_input("release_tag=2.8.25").unwrap();
        params.add_input("mode=candidate").unwrap();
        params
            .add_matrix("target:x86_64-unknown-linux-musl")
            .unwrap();
        params.add_env("PYTEST_ADDOPTS=-s").unwrap();
        params
    }

    #[test]
    fn clud_installer_lane_parses_validates_and_becomes_act_args() {
        let params = installer();
        params.validate(Trigger::WorkflowCall).unwrap();
        params.validate(Trigger::WorkflowDispatch).unwrap();
        assert_eq!(
            params.act_args(),
            [
                "--input",
                "mode=candidate",
                "--input",
                "release_tag=2.8.25",
                "--matrix",
                "target:x86_64-unknown-linux-musl",
                "--env",
                "PYTEST_ADDOPTS=-s",
            ]
        );
        assert_eq!(
            params.describe().unwrap(),
            "inputs: mode=candidate, release_tag=2.8.25; \
             matrix: target:x86_64-unknown-linux-musl; env: PYTEST_ADDOPTS=-s"
        );
        assert_eq!(RunParams::default().describe(), None);
        assert!(RunParams::default().act_args().is_empty());
        // Values keep their own separators: only the first splits.
        let mut params = RunParams::default();
        params.add_env("FLAGS=a=b").unwrap();
        assert_eq!(params.env["FLAGS"], "a=b");
    }

    #[test]
    fn act_receives_them_after_the_job_with_no_secret_in_argv() {
        let args = crate::ci::engine::ActInvocation {
            event: "workflow_call".into(),
            workflow: ".github/workflows/installer-check.yml".into(),
            workflow_overlaid: false,
            job: Some("public-host".into()),
            cache_namespace: "0123456789abcdef".into(),
            secrets: Default::default(),
            params: installer(),
        }
        .args();
        assert_eq!(args[0], "workflow_call");
        let tail: Vec<&str> = args[args.len() - 10..].iter().map(String::as_str).collect();
        assert_eq!(
            tail,
            [
                "-j",
                "public-host",
                "--input",
                "mode=candidate",
                "--input",
                "release_tag=2.8.25",
                "--matrix",
                "target:x86_64-unknown-linux-musl",
                "--env",
                "PYTEST_ADDOPTS=-s",
            ]
        );
    }

    #[test]
    fn malformed_or_repeated_specs_are_refused() {
        let mut params = RunParams::default();
        for bad in ["novalue", "=x", ""] {
            assert!(params.add_input(bad).is_err(), "{bad:?}");
            assert!(params.add_env(bad).is_err(), "{bad:?}");
        }
        assert!(params.add_matrix("target=x").is_err());
        params.add_input("a=1").unwrap();
        assert!(params.add_input("a=2").unwrap_err().contains("given twice"));
    }

    #[test]
    fn inputs_need_an_event_that_takes_them() {
        let params = installer();
        for trigger in [Trigger::Pr, Trigger::Push, Trigger::Release] {
            let refused = params.validate(trigger).unwrap_err();
            assert!(refused.contains("--event"), "{refused}");
        }
        // A matrix filter and env work with any trigger.
        let RunParams { matrix, env, .. } = installer();
        let rest = RunParams {
            matrix,
            env,
            ..RunParams::default()
        };
        rest.validate(Trigger::Pr).unwrap();
    }

    #[test]
    fn keys_values_and_sizes_are_bounded() {
        let with = |f: fn(&mut RunParams, &str) -> Result<(), String>, spec: &str| {
            let mut params = RunParams::default();
            f(&mut params, spec)?;
            params.validate(Trigger::WorkflowCall)
        };
        for spec in [
            "1abc=x",
            "a b=x",
            "a.b=x",
            "a=line\nbreak",
            &format!("{}=x", "k".repeat(MAX_KEY + 1)),
            &format!("k={}", "v".repeat(MAX_VALUE + 1)),
        ] {
            assert!(with(RunParams::add_input, spec).is_err(), "{spec:?}");
        }
        assert!(with(RunParams::add_input, "dash-ok_1=x").is_ok());
        assert!(with(RunParams::add_matrix, "os:a:b").is_err());
        let mut many = RunParams::default();
        for n in 0..=MAX_ENTRIES {
            many.add_env(&format!("V{n}=x")).unwrap();
        }
        assert!(many.validate(Trigger::Push).is_err());
    }

    #[test]
    fn secrets_and_runner_names_never_travel_through_env() {
        for key in [
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "npm_token",
            "AWS_SECRET_ACCESS_KEY",
            "DB_PASSWORD",
            "GITHUB_API_URL",
            "ACTIONS_RUNTIME_URL",
            "RUNNER_TEMP",
            "SSH_PRIVATE_KEY",
            "dash-name",
        ] {
            let mut params = RunParams::default();
            params.add_env(&format!("{key}=x")).unwrap();
            assert!(params.validate(Trigger::Push).is_err(), "{key}");
        }
        let mut params = RunParams::default();
        params.add_env("PYTEST_ADDOPTS=-s").unwrap();
        params.add_env("RUST_LOG=debug").unwrap();
        params.validate(Trigger::Push).unwrap();
    }

    #[test]
    fn empty_params_serialize_to_nothing_and_round_trip() {
        assert_eq!(serde_json::to_string(&RunParams::default()).unwrap(), "{}");
        let params = installer();
        let json = serde_json::to_string(&params).unwrap();
        assert_eq!(serde_json::from_str::<RunParams>(&json).unwrap(), params);
        assert!(serde_json::from_str::<RunParams>(r#"{"secrets":{}}"#).is_err());
    }
}
