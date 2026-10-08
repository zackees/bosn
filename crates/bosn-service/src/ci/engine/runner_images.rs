//! Shared runner image archive loading and recovery.
use super::{ENGINE_CACHE, RUNNER_IMAGE, runner_tag};

/// The runner image tar in the cache volume.
pub(super) fn runner_tar() -> String {
    format!(
        "{ENGINE_CACHE}/images/{}.tar",
        runner_tag().replace([':', '/'], "-")
    )
}

/// Shell that loads the runner image tar from the cache volume, or pulls
/// the pinned image, tags it [`runner_tag`] and saves the tar atomically.
pub(super) fn load_runner_script() -> String {
    format!("{} {}", runner_input_lock(true), load_runner_body())
}

fn runner_input_lock(shared: bool) -> String {
    format!(
        "mkdir -p {ENGINE_CACHE}/images; exec 9>>{}.lock; flock -{} 9;",
        runner_tar(),
        if shared { "s" } else { "x" }
    )
}

pub(super) fn load_runner_body() -> String {
    let tag = runner_tag();
    let tar = runner_tar();
    let restore = "[ -f \"$tar\" ] && docker load -q -i \"$tar\" >/dev/null";
    format!(
        "tar={tar}; mkdir -p {ENGINE_CACHE}/images; \
         if ! {{ {restore}; }}; then \
           flock -x 9; \
           if ! {{ {restore}; }}; then \
           docker pull -q --platform linux/amd64 {RUNNER_IMAGE} >/dev/null && \
           docker tag {RUNNER_IMAGE} {tag} && \
           stage=$(mktemp \"$tar.XXXXXXXX\") && \
           trap 'rm -f \"$stage\"' EXIT && \
           docker save --platform linux/amd64 -o \"$stage\" {tag} && mv \"$stage\" \"$tar\" || exit 1; \
           fi; \
         fi; \
         docker image inspect {tag} >/dev/null"
    )
}

/// After a cached tar failed the runner proof: drop it and the image it
/// loaded, then pull and save again.
pub(super) fn reload_runner_script() -> String {
    format!(
        "{lock} rm -f {tar}; docker image rm -f {tag} >/dev/null 2>&1 || :; {load}",
        lock = runner_input_lock(false),
        tar = runner_tar(),
        tag = runner_tag(),
        load = load_runner_body(),
    )
}
