//! The runner images a run may use beyond the default one, read from the
//! run's source snapshot and made ready inside the engine.
//!
//! The table itself ([`RunnerTable`]) is pure and lives in bosn-core. Here a
//! run's table is loaded from its frozen snapshot (the built-in table plus
//! the repository's [`DECLARATION_PATH`]), and each non-default image is
//! loaded from the cache volume or produced once and saved there, with the
//! same proof the default image gets: `docker image inspect` must show the
//! image ID recorded when the image was produced from its pinned source.

use std::path::Path;

use bosn_core::runner_images::{DECLARATION_PATH, RunnerSource, RunnerTable};

use super::ENGINE_CACHE;

/// The run's runner table: the built-in one plus the declaration in the
/// snapshot at `root`, if any. A malformed declaration refuses the run.
pub fn load_table(root: &Path) -> Result<RunnerTable, String> {
    let read = |relative: &str| -> Result<Option<String>, String> {
        match std::fs::read_to_string(root.join(relative)) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("{relative}: {error}")),
        }
    };
    match read(DECLARATION_PATH)? {
        Some(text) => RunnerTable::with_declaration(&text, read),
        None => Ok(RunnerTable::builtin()),
    }
}

/// The labels the run maps: the default ones and every other entry the
/// snapshot's workflows name.
pub fn runners(root: &Path) -> Result<Vec<(String, RunnerSource)>, String> {
    let table = load_table(root)?;
    let texts = workflow_texts(root);
    Ok(super::runners_for(&table, texts.iter().map(String::as_str)))
}

fn workflow_texts(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root.join(".github/workflows")) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "yml" || e == "yaml"))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect()
}

/// One engine script per distinct non-default image the run maps; the
/// default image is `act serve prepare`'s.
pub fn ensure_scripts(runners: &[(String, RunnerSource)]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    runners
        .iter()
        .filter(|(_, source)| !source.is_default() && seen.insert(source.tag()))
        .map(|(_, source)| ensure_script(source))
        .collect()
}

/// Make `source`'s image present in the engine under its tag, proven:
/// - the image tar and its record (`<source digest> <image id>`) live in the
///   cache volume's `images/`, so a warm engine only loads it;
/// - a loaded or present image is used only when `docker image inspect`
///   shows the recorded image ID; a cached tar that fails is discarded;
/// - otherwise the image is produced from its pinned source: pulled by
///   manifest digest (and its repo digest checked), or built from the
///   Dockerfile with no context and labelled with the Dockerfile's digest,
///   then saved and recorded.
///
/// One lock per engine serialises producers, so concurrent runs build once.
pub fn ensure_script(source: &RunnerSource) -> String {
    use base64::Engine as _;
    let tag = source.tag();
    let key = source.digest();
    let produce = match source {
        RunnerSource::Pinned { reference } => format!(
            "docker pull --quiet --platform linux/amd64 '{reference}' >/dev/null; \
             docker image inspect --format '{{{{json .RepoDigests}}}}' '{reference}' | grep -qF '{key}' \
               || {{ echo 'runner image {reference} does not carry its pinned digest' >&2; exit 1; }}; \
             docker tag '{reference}' \"$tag\""
        ),
        RunnerSource::Dockerfile { origin, contents } => format!(
            "echo 'bosn: building runner image {tag} from {origin} (once per engine and Dockerfile digest)' >&2; \
             printf %s '{}' | base64 -d | docker build --quiet --platform linux/amd64 \
               --label com.zackees.bosn.runner.source={key} -t \"$tag\" - >/dev/null",
            base64::engine::general_purpose::STANDARD.encode(contents)
        ),
    };
    let archive = tag.replace([':', '/'], "-");
    format!(
        "set -eu; dir={ENGINE_CACHE}/images; mkdir -p \"$dir\"; \
         exec 8>\"$dir/.runner-images.lock\"; flock -x 8; \
         tag='{tag}'; key='{key}'; tar=\"$dir/{archive}.tar\"; record=\"$tar.id\"; \
         proven() {{ id=$(docker image inspect --format '{{{{.Id}}}}' \"$tag\" 2>/dev/null) || return 1; \
           [ -f \"$record\" ] && [ \"$(cat \"$record\")\" = \"$key $id\" ]; }}; \
         if proven; then exit 0; fi; \
         if [ -f \"$tar\" ] && [ -f \"$record\" ]; then \
           if docker load --quiet -i \"$tar\" >/dev/null && proven; then exit 0; fi; \
           echo \"bosn: cached runner image $tag failed its proof; producing it again\" >&2; \
           rm -f \"$tar\" \"$record\"; docker image rm -f \"$tag\" >/dev/null 2>&1 || true; \
         fi; \
         {produce}; \
         id=$(docker image inspect --format '{{{{.Id}}}}' \"$tag\"); \
         docker save -o \"$tar.partial\" \"$tag\"; mv \"$tar.partial\" \"$tar\"; \
         printf '%s %s' \"$key\" \"$id\" > \"$record.partial\"; mv \"$record.partial\" \"$record\""
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(files: &[(&str, &str)]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "bosn-runner-images-{}-{}",
            std::process::id(),
            files.len()
        ));
        if root.exists() {
            std::fs::remove_dir_all(&root).unwrap();
        }
        for (path, text) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        root
    }

    #[test]
    fn a_run_maps_the_default_labels_and_only_the_extra_ones_it_names() {
        let root = snapshot(&[(
            ".github/workflows/ci.yml",
            "jobs:\n  a:\n    runs-on: ubuntu-26.04\n    steps: [{run: cat /etc/os-release}]\n",
        )]);
        let labels: Vec<_> = runners(&root)
            .unwrap()
            .into_iter()
            .map(|(l, _)| l)
            .collect();
        assert_eq!(
            labels,
            [
                "ubuntu-22.04",
                "ubuntu-24.04",
                "ubuntu-26.04",
                "ubuntu-latest"
            ]
        );
        let scripts = ensure_scripts(&runners(&root).unwrap());
        assert_eq!(scripts.len(), 1, "only ubuntu-26.04 needs producing");
        assert!(scripts[0].contains("docker build"));
        assert!(scripts[0].contains("com.zackees.bosn.runner.source=sha256:"));
        assert!(scripts[0].contains("docker save"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_declared_runner_is_read_from_the_snapshot() {
        let digest = "1".repeat(64);
        let root = snapshot(&[
            (
                DECLARATION_PATH,
                &format!("[runners.gpu]\nimage = \"ghcr.io/o/r@sha256:{digest}\"\n"),
            ),
            (".github/workflows/x.yaml", "jobs: {a: {runs-on: [gpu]}}\n"),
            (
                ".github/workflows/y.yaml",
                "jobs: {a: {runs-on: ubuntu-26.04-arm}}\n",
            ),
        ]);
        let labels: Vec<_> = runners(&root)
            .unwrap()
            .into_iter()
            .map(|(l, _)| l)
            .collect();
        assert_eq!(
            labels,
            ["gpu", "ubuntu-22.04", "ubuntu-24.04", "ubuntu-latest"]
        );
        let scripts = ensure_scripts(&runners(&root).unwrap());
        assert!(scripts[0].contains(&format!(
            "docker pull --quiet --platform linux/amd64 'ghcr.io/o/r@sha256:{digest}'"
        )));
        std::fs::write(
            root.join(DECLARATION_PATH),
            "[runners.gpu]\nimage = \"ghcr.io/o/r:latest\"\n",
        )
        .unwrap();
        assert!(runners(&root).unwrap_err().contains("pinned by digest"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn without_a_workflow_naming_them_nothing_extra_is_produced() {
        assert!(ensure_scripts(&super::super::default_runners()).is_empty());
    }
}
