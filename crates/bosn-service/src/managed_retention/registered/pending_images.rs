//! Fresh immutable repository evidence scopes opted-out pending pull protection.

use super::super::*;

pub(super) fn protects_pull(
    engine: &DockerEngine,
    intent: &bosn_registry::ImageCreationIntent,
    image_id: &str,
) -> bool {
    let observation = parse_read::<ImageDetail>(engine.inspect_images(
        &[image_id.to_owned()],
        budget::options(RunOptions::bounded(
            RETENTION_READ_DEADLINE,
            RETENTION_OUTPUT_LIMIT,
        )),
    ));
    let Some(entries) = observation else {
        return true;
    };
    let [image] = entries.as_slice() else {
        return true;
    };
    if image.id != image_id {
        return true;
    }
    let Some(repo_digests) = &image.repo_digests else {
        return true;
    };
    intent.matches_pull_repository(repo_digests).unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_pull_protects_exact_digest_and_incomplete_reads() {
        let identity = format!("sha256:{}", "a".repeat(64));
        let intent = bosn_registry::ImageCreationIntent {
            reference: format!("alpine@sha256:{}", "b".repeat(64)),
            content_sha256: "c".repeat(64),
            workspace: "/workspace".into(),
            stack: "app".into(),
            owner: bosn_registry::ImageIntentOwner::Setup,
            source: bosn_registry::ImageIntentSource::Pull,
            created_at: 1.0,
        };
        for (document, protected) in [
            (serde_json::json!([{"Id": identity, "RepoDigests": [intent.reference], "Created": "2026-10-01T00:00:00Z"}]).to_string(), true),
            (serde_json::json!([{"Id": identity, "RepoDigests": [format!("other@sha256:{}", "b".repeat(64))], "Created": "2026-10-01T00:00:00Z"}]).to_string(), false),
            ("[]".into(), true), ("invalid".into(), true),
        ] {
            let engine = DockerEngine::synthetic_for_test("/bin/sh", ["-c", "printf '%s' \"$BOSN_PENDING_IMAGE\"", "fixture"])
                .env("BOSN_PENDING_IMAGE", document);
            assert_eq!(protects_pull(&engine, &intent, &identity), protected);
        }
    }
}
