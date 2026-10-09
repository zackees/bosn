//! Stable coordination outside disposable archive directories.
use super::ENGINE_CACHE;

/// Take this before opening a per-file archive lock. Waiting readers retain
/// their shared descriptor until the entire download/extraction/load exits.
pub(super) fn reader() -> String {
    format!(
        "[ -d {ENGINE_CACHE} ] && [ ! -L {ENGINE_CACHE} ] || exit 78; \
         artifact_lock={ENGINE_CACHE}/.artifact-cache.lock; \
         [ ! -L \"$artifact_lock\" ] && {{ [ ! -e \"$artifact_lock\" ] || [ -f \"$artifact_lock\" ]; }} || exit 78; \
         exec 5>>\"$artifact_lock\"; flock -s 5 || exit $?;"
    )
}

pub(super) fn maintenance_archive() -> String {
    format!("{ENGINE_CACHE}/.act-maintenance-archive-v1.tgz")
}

pub(super) fn prefer_preserved_archive(sha256: &str) -> String {
    format!(
        "preserved={archive}; if [ -f \"$preserved\" ] && [ ! -L \"$preserved\" ] && echo \"{sha256}  $preserved\" | sha256sum -c - >/dev/null 2>&1; then tgz=$preserved; fi;",
        archive = maintenance_archive()
    )
}
