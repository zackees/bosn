//! Engine reads and removal for Bosn-built setup and manifest images (#545).

use std::borrow::Cow;

use bosn_engine::{CensusRead, CommandError, DockerEngine, RunOptions};

use super::registered::images::{BUILT_IMAGE_REPOSITORY, only_built_tags};
use super::{ImageDetail, RETENTION_OUTPUT_LIMIT, RETENTION_READ_DEADLINE, parse_read};

/// The listing probe that stands for "every `bosn-setup` tag" rather than a label key.
pub(super) const BUILT_REFERENCE_PROBE: &str = "reference:bosn-setup";

/// List image ids for one probe: a label key, or [`BUILT_REFERENCE_PROBE`].
pub(super) fn image_ids(
    engine: &DockerEngine,
    probe: &str,
    options: RunOptions,
) -> Result<CensusRead, CommandError> {
    if probe == BUILT_REFERENCE_PROBE {
        engine.image_ids_with_reference(BUILT_IMAGE_REPOSITORY, options)
    } else {
        engine.image_ids_with_label(probe, options)
    }
}

/// The `docker rmi` argv for one image.
///
/// One content can carry several `bosn-setup:` tags (two documents with one Dockerfile), and a
/// plain `rmi <id>` refuses such an image. Removing exactly its Bosn tags deletes it when they
/// are all it has; a tag someone adds in the meantime survives and keeps the image. Never `-f`.
pub(super) fn removal_argv<'a>(engine: &DockerEngine, id: &'a str) -> Vec<Cow<'a, str>> {
    let options = RunOptions::bounded(RETENTION_READ_DEADLINE, RETENTION_OUTPUT_LIMIT);
    let tags = parse_read::<ImageDetail>(engine.inspect_images(&[id.to_owned()], options))
        .and_then(|entries| entries.into_iter().next())
        .filter(|entry| entry.id == id && entry.repo_tags.len() > 1)
        .map(|entry| entry.repo_tags)
        .filter(|tags| only_built_tags(tags));
    let mut argv = vec![Cow::Borrowed("rmi")];
    match tags {
        Some(tags) => argv.extend(tags.into_iter().map(Cow::Owned)),
        None => argv.push(Cow::Borrowed(id)),
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::only_built_tags;

    #[test]
    fn only_content_addressed_bosn_setup_tags_prove_a_built_image() {
        let built = format!("bosn-setup:{}", "a".repeat(64));
        assert!(only_built_tags(std::slice::from_ref(&built)));
        assert!(only_built_tags(&[
            built.clone(),
            format!("bosn-setup:{}", "b".repeat(64))
        ]));
        assert!(!only_built_tags(&[]));
        assert!(!only_built_tags(&[built.clone(), "python:3.12".into()]));
        assert!(!only_built_tags(&["bosn-setup:latest".into()]));
        assert!(!only_built_tags(&[format!(
            "bosn-setup:{}",
            "A".repeat(64)
        )]));
        assert!(!only_built_tags(&[format!(
            "example/bosn-setup:{}",
            "a".repeat(64)
        )]));
    }
}
