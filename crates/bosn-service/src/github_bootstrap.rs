//! The first step bosn adds to every job of a `--github-proxy` run (#650).
//!
//! It runs in the job container before any of the workflow's own steps and
//! sets up, for every later step (through `$GITHUB_ENV`):
//!
//! - `HTTPS_PROXY`/`https_proxy`: the run's relay in the engine, reached at
//!   the container's default gateway on the port act passes as
//!   [`PORT_ENV`]. Tools that hard-code `https://api.github.com` (`gh`,
//!   ci-lint's urllib, octokit) honour it, unlike `GITHUB_API_URL`;
//! - trust for the run's CA: `SSL_CERT_FILE`, `CURL_CA_BUNDLE`,
//!   `REQUESTS_CA_BUNDLE` and `GIT_SSL_CAINFO` name the system bundle plus
//!   the CA, `NODE_EXTRA_CA_CERTS` the CA alone.
//!
//! The CA certificate is public; its key never leaves the daemon. Nothing
//! secret is in the step.

use std::io;
use std::path::Path;

use serde_yaml::{Mapping, Value};

/// The act `--env` that carries the relay's job port.
pub const PORT_ENV: &str = "BOSN_GITHUB_PROXY_PORT";
/// The step's name, which also marks it so it is added once.
pub const STEP_NAME: &str = "bosn: route api.github.com through the run's GitHub proxy";

/// The bootstrap step for a run whose CA is `ca_pem`.
#[must_use]
pub fn step(ca_pem: &str) -> Value {
    let script = format!(
        r#"set -eu
d="${{RUNNER_TEMP:-/tmp}}/bosn-github-proxy"
mkdir -p "$d"
cat > "$d/ca.pem" <<'BOSN_CA'
{pem}
BOSN_CA
gw=$(awk '$2 == "00000000" {{ print $3; exit }}' /proc/net/route)
[ -n "$gw" ] || {{ echo "bosn: no default route, so the run's GitHub proxy is unreachable" >&2; exit 1; }}
ip=$(printf '%d.%d.%d.%d' "0x${{gw:6:2}}" "0x${{gw:4:2}}" "0x${{gw:2:2}}" "0x${{gw:0:2}}")
proxy="http://$ip:${{{PORT_ENV}}}"
sys=
for f in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt /etc/ssl/cert.pem; do
  if [ -r "$f" ]; then sys=$f; break; fi
done
{{ if [ -n "$sys" ]; then cat "$sys"; fi; cat "$d/ca.pem"; }} > "$d/bundle.pem"
{{
  echo "HTTPS_PROXY=$proxy"
  echo "https_proxy=$proxy"
  echo "NO_PROXY=localhost,127.0.0.1,::1"
  echo "no_proxy=localhost,127.0.0.1,::1"
  echo "SSL_CERT_FILE=$d/bundle.pem"
  echo "CURL_CA_BUNDLE=$d/bundle.pem"
  echo "REQUESTS_CA_BUNDLE=$d/bundle.pem"
  echo "GIT_SSL_CAINFO=$d/bundle.pem"
  echo "NODE_EXTRA_CA_CERTS=$d/ca.pem"
}} >> "$GITHUB_ENV"
echo "bosn: api.github.com goes through this run's GitHub proxy ($proxy); GITHUB_TOKEN is a per-run stand-in the proxy swaps for the host's credential"
"#,
        pem = ca_pem.trim()
    );
    let mut step = Mapping::new();
    step.insert("name".into(), STEP_NAME.into());
    step.insert("shell".into(), "bash".into());
    step.insert("run".into(), script.into());
    Value::Mapping(step)
}

/// Prepend `step` to every job with `steps` in every workflow of the
/// snapshot at `source`, writing the result to `overlay` (reading the
/// overlay's copy when bosn already rewrote the file). Returns the number of
/// jobs given the step.
pub fn inject(source: &Path, overlay: &Path, step: &Value) -> io::Result<usize> {
        let _ = (source, overlay, step);
        Ok(0) // RED: stub
    }

fn prepend(document: &mut Value, step: &Value) -> usize {
    let Some(jobs) = document.get_mut("jobs").and_then(Value::as_mapping_mut) else {
        return 0;
    };
    let mut added = 0;
    for (_, job) in jobs.iter_mut() {
        let Some(steps) = job.get_mut("steps").and_then(Value::as_sequence_mut) else {
            continue;
        };
        let present = steps
            .first()
            .and_then(|s| s.get("name"))
            .and_then(Value::as_str)
            == Some(STEP_NAME);
        if !present {
            steps.insert(0, step.clone());
            added += 1;
        }
    }
    added
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIBfake\n-----END CERTIFICATE-----\n";

    #[test]
    fn every_job_with_steps_gets_the_step_first_once() {
        let dir = tempfile::tempdir().unwrap();
        let (source, overlay) = (dir.path().join("src"), dir.path().join("overlay"));
        std::fs::create_dir_all(source.join(".github/workflows")).unwrap();
        std::fs::write(
            source.join(".github/workflows/ci-pre.yml"),
            "on: [pull_request]\njobs:\n  cache-janitor:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo one\n  called:\n    uses: ./.github/workflows/other.yml\n",
        )
        .unwrap();
        // A file bosn already rewrote is read from the overlay.
        std::fs::create_dir_all(overlay.join(".github/workflows")).unwrap();
        std::fs::write(
            source.join(".github/workflows/b.yaml"),
            "jobs:\n  x:\n    steps: [{run: original}]\n",
        )
        .unwrap();
        std::fs::write(
            overlay.join(".github/workflows/b.yaml"),
            "jobs:\n  x:\n    steps: [{run: rewritten}]\n",
        )
        .unwrap();
        let step = step(PEM);
        assert_eq!(inject(&source, &overlay, &step).unwrap(), 2);
        assert_eq!(inject(&source, &overlay, &step).unwrap(), 0, "idempotent");
        let written: Value = serde_yaml::from_str(
            &std::fs::read_to_string(overlay.join(".github/workflows/ci-pre.yml")).unwrap(),
        )
        .unwrap();
        let steps = written["jobs"]["cache-janitor"]["steps"]
            .as_sequence()
            .unwrap();
        assert_eq!(steps[0]["name"].as_str(), Some(STEP_NAME));
        assert_eq!(steps[1]["run"].as_str(), Some("echo one"));
        assert!(written["jobs"]["called"].get("steps").is_none());
        let b = std::fs::read_to_string(overlay.join(".github/workflows/b.yaml")).unwrap();
        assert!(b.contains("rewritten") && !b.contains("original"));
        // The source snapshot is never touched.
        assert!(
            !std::fs::read_to_string(source.join(".github/workflows/ci-pre.yml"))
                .unwrap()
                .contains(STEP_NAME)
        );
    }

    #[test]
    fn the_step_trusts_the_ca_and_routes_https_through_the_relay() {
        let step = step(PEM);
        let run = step["run"].as_str().unwrap();
        assert!(run.contains("MIIBfake"));
        assert!(run.contains("${BOSN_GITHUB_PROXY_PORT}"));
        for name in [
            "HTTPS_PROXY=",
            "https_proxy=",
            "SSL_CERT_FILE=",
            "NODE_EXTRA_CA_CERTS=",
            "GIT_SSL_CAINFO=",
        ] {
            assert!(run.contains(name), "{name}");
        }
        assert!(
            !run.contains("GITHUB_TOKEN="),
            "the step never sets a token"
        );
        assert_eq!(step["shell"].as_str(), Some("bash"));
    }
}
