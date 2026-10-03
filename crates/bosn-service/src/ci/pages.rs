//! `actions/configure-pages` served locally (GATE-012 refinement).
//!
//! A Pages *build* job calls `actions/configure-pages` only to read the
//! site's metadata, then builds, validates and uploads the site with
//! `actions/upload-pages-artifact`. That work is local-safe; only
//! `actions/deploy-pages` needs GitHub. So in the run's copy of a workflow
//! bosn replaces each configure-pages step that
//! [`super::remote_only::classify`] calls [`StepClass::Stubbed`] with a
//! `run:` step that writes the action's outputs for the repository's GitHub
//! Pages site:
//!
//! | output      | project site `owner/repo`      | user site `owner/owner.github.io` |
//! |-------------|--------------------------------|-----------------------------------|
//! | `base_url`  | `https://owner.github.io/repo` | `https://owner.github.io`         |
//! | `origin`    | `https://owner.github.io`      | `https://owner.github.io`         |
//! | `host`      | `owner.github.io`              | `owner.github.io`                 |
//! | `base_path` | `/repo`                        | (empty)                           |
//!
//! The step keeps its `id`, `name`, `if:` and error handling, so later steps
//! read `steps.<id>.outputs.*` as on GitHub. A custom domain is not known
//! locally; the project-site URL stands in for it.

use serde_yaml::{Mapping, Value};

use super::remote_only::{StepClass, classify};

/// Step keys the stub keeps from the step it replaces.
const KEPT: &[&str] = &["id", "name", "if", "continue-on-error", "timeout-minutes"];

/// The four outputs of `actions/configure-pages` for `repository`
/// (`owner/name`), or `None` when it is not a plain GitHub name.
pub fn site(repository: &str) -> Option<[(&'static str, String); 4]> {
    let (owner, name) = repository.split_once('/')?;
    let plain = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    if !plain(owner) || !plain(name) || name.contains("..") {
        return None;
    }
    let host = format!("{}.github.io", owner.to_ascii_lowercase());
    let origin = format!("https://{host}");
    let base_path = if name.eq_ignore_ascii_case(&host) {
        String::new()
    } else {
        format!("/{name}")
    };
    Some([
        ("base_url", format!("{origin}{base_path}")),
        ("origin", origin),
        ("host", host),
        ("base_path", base_path),
    ])
}

/// Replace every stubbable configure-pages step of every job in a workflow
/// document; returns the IDs of the jobs changed (once per step).
pub fn stub_configure_pages(document: &mut Value, repository: &str) -> Vec<String> {
    let Some(outputs) = site(repository) else {
        return Vec::new();
    };
    let Some(jobs) = document.get_mut("jobs").and_then(Value::as_mapping_mut) else {
        return Vec::new();
    };
    let mut stubbed = Vec::new();
    for (id, job) in jobs.iter_mut() {
        let Some(steps) = job.get_mut("steps").and_then(Value::as_sequence_mut) else {
            continue;
        };
        for step in steps.iter_mut() {
            if stubbable(step) {
                *step = Value::Mapping(stub(step, &outputs));
                stubbed.push(id.as_str().unwrap_or_default().to_string());
            }
        }
    }
    stubbed
}

fn stubbable(step: &Value) -> bool {
    let Some(uses) = step.get("uses").and_then(Value::as_str) else {
        return false;
    };
    let with = step
        .get("with")
        .and_then(|with| serde_yaml::from_value(with.clone()).ok())
        .unwrap_or_default();
    classify(uses, &with) == StepClass::Stubbed
}

fn stub(step: &Value, outputs: &[(&'static str, String); 4]) -> Mapping {
    let mut stub = Mapping::new();
    for key in KEPT {
        if let Some(value) = step.get(*key) {
            stub.insert((*key).into(), value.clone());
        }
    }
    if !stub.contains_key("name") {
        stub.insert("name".into(), step.get("uses").cloned().unwrap_or_default());
    }
    stub.insert("shell".into(), "bash".into());
    // `site` admits only [A-Za-z0-9._-/:] into the values: safe in quotes.
    let lines: Vec<String> = outputs
        .iter()
        .map(|(key, value)| format!("echo '{key}={value}'"))
        .collect();
    stub.insert(
        "run".into(),
        format!(
            "echo 'bosn ci: actions/configure-pages stubbed locally (GATE-012)'\n{{\n{}\n}} >> \"$GITHUB_OUTPUT\"\n",
            lines.join("\n")
        )
        .into(),
    );
    stub
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outputs(repository: &str) -> Vec<(String, String)> {
        site(repository)
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    #[test]
    fn project_and_user_sites_get_github_pages_urls() {
        let pairs = |list: [(&str, &str); 4]| {
            list.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            outputs("Zackees/clud"),
            pairs([
                ("base_url", "https://zackees.github.io/clud"),
                ("origin", "https://zackees.github.io"),
                ("host", "zackees.github.io"),
                ("base_path", "/clud"),
            ])
        );
        assert_eq!(
            outputs("zackees/zackees.github.io"),
            pairs([
                ("base_url", "https://zackees.github.io"),
                ("origin", "https://zackees.github.io"),
                ("host", "zackees.github.io"),
                ("base_path", ""),
            ])
        );
        for bad in ["noslash", "a/", "/b", "a/b c", "a/b'c", "a/..", "a/$(x)"] {
            assert!(site(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn configure_pages_becomes_a_stub_that_sets_its_outputs() {
        let mut document: Value = serde_yaml::from_str(
            "on: [push]\njobs:\n  build-site:\n    runs-on: ubuntu-latest\n    steps:\n      - run: make site\n      - id: pages\n        uses: actions/configure-pages@v5\n        if: github.event_name != 'pull_request'\n      - run: echo ${{ steps.pages.outputs.base_url }}\n      - uses: actions/upload-pages-artifact@v3\n        with: {path: site}\n  generated:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/configure-pages@v5\n        with: {static_site_generator: next}\n",
        )
        .unwrap();
        assert_eq!(
            stub_configure_pages(&mut document, "zackees/clud"),
            ["build-site"]
        );
        let step = &document["jobs"]["build-site"]["steps"][1];
        assert_eq!(step["id"], "pages");
        assert_eq!(step["if"], "github.event_name != 'pull_request'");
        assert_eq!(step["name"], "actions/configure-pages@v5");
        assert_eq!(step["shell"], "bash");
        assert!(step.get("uses").is_none());
        let run = step["run"].as_str().unwrap();
        for line in [
            "echo 'base_url=https://zackees.github.io/clud'",
            "echo 'origin=https://zackees.github.io'",
            "echo 'host=zackees.github.io'",
            "echo 'base_path=/clud'",
            "} >> \"$GITHUB_OUTPUT\"",
        ] {
            assert!(run.contains(line), "{run}");
        }
        // upload-pages-artifact runs as written, against act's artifact server.
        assert_eq!(
            document["jobs"]["build-site"]["steps"][3]["uses"],
            "actions/upload-pages-artifact@v3"
        );
        // A generator-configuring step cannot be stubbed: its job is confined.
        assert_eq!(
            document["jobs"]["generated"]["steps"][0]["uses"],
            "actions/configure-pages@v5"
        );
        let confined = super::super::remote_only::confine(&mut document);
        assert_eq!(confined.len(), 1);
        assert_eq!(confined[0].0, "generated");
        assert!(
            confined[0].1.contains("static_site_generator"),
            "{confined:?}"
        );
        // No repository name to derive a site from: nothing is stubbed.
        let mut untouched: Value =
            serde_yaml::from_str("jobs:\n  a:\n    steps: [{uses: actions/configure-pages@v5}]\n")
                .unwrap();
        assert!(stub_configure_pages(&mut untouched, "not a repo").is_empty());
    }
}
