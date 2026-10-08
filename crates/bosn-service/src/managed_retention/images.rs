//! Inspect registered image digests, including images with immutable upstream labels.

use super::*;

pub(super) fn observe_registered(
    engine: &DockerEngine,
    ownership: &registered::RegisteredOwnership,
    options: RunOptions,
    artifacts: &mut Vec<bosn_core::ObservedArtifact>,
    unreadable: &mut details::ReadFailures,
) {
    let owned = ownership.image_ids();
    if owned.is_empty() {
        return;
    }
    if !budget::observe(unreadable) {
        return;
    }
    let result = match engine
        .with_args(["image", "ls", "-a", "-q", "--no-trunc"])
        .capture(budget::options(options))
    {
        Ok(result) if result.ok() => result,
        _ => {
            unreadable.push("registered image inventory unavailable".into());
            return;
        }
    };
    let Ok(text) = std::str::from_utf8(&result.stdout) else {
        unreadable.push("registered image inventory is not UTF-8".into());
        return;
    };
    let mut ids = std::collections::BTreeSet::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let id = if line.starts_with("sha256:") {
            line.to_owned()
        } else {
            format!("sha256:{line}")
        };
        if id.len() != 71 || !id[7..].bytes().all(|byte| byte.is_ascii_hexdigit()) {
            unreadable.push("registered image inventory contains an invalid digest".into());
            return;
        }
        if owned.contains(&id) {
            ids.insert(id);
        }
    }
    for id in ids {
        if !budget::observe(unreadable) {
            return;
        }
        let Some(entries) = parse_inspect::<ImageDetail>(
            engine.inspect_images(std::slice::from_ref(&id), budget::options(options)),
            "registered image inspect",
            unreadable,
        ) else {
            continue;
        };
        for entry in entries {
            if entry.id != id {
                unreadable.push("registered image inspection returned a different identity".into());
                continue;
            }
            artifacts.push(bosn_core::ObservedArtifact {
                id: entry.id.clone(),
                kind: ResourceKind::Image,
                labels: entry.labels(),
                signals: signals(!image_is_unused(
                    engine,
                    &entry.id,
                    budget::options(options),
                )),
                bytes: entry.size_bytes(),
                age_seconds: entry.created_age(now_seconds()),
            });
        }
    }
}
