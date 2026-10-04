//! Shared digest-verified act installation for bootstrap and readiness.

use super::{ACT_VERSION, ActArtifact, ENGINE_CACHE, ENGINE_WORK};

/// Shell (busybox) that installs act from the cache volume, refreshing a
/// missing or corrupt tarball from the pinned URL; prints `act --version`.
pub(crate) fn install_act_script(act: ActArtifact) -> String {
    format!(
        "tgz={archive}; mkdir -p {ENGINE_CACHE}/tools; \
         exec 9>>\"$tgz.lock\"; flock -x 9; \
         if ! echo \"{sum}  $tgz\" | sha256sum -c - >/dev/null 2>&1; then \
           stage=$(mktemp \"$tgz.XXXXXXXX\"); trap 'rm -f \"$stage\"' EXIT; \
           wget -q -O \"$stage\" '{url}' && \
           echo \"{sum}  $stage\" | sha256sum -c - >/dev/null && mv \"$stage\" \"$tgz\" || exit 1; \
         fi; \
         tar -xzf \"$tgz\" -C {ENGINE_WORK}/bin act && \
         echo \"{binary}  {ENGINE_WORK}/bin/act\" | sha256sum -c - >/dev/null && \
         {ENGINE_WORK}/bin/act --version || exit $?",
        url = act.url,
        sum = act.sha256,
        binary = act.binary_sha256,
        archive = act_archive(act),
    )
}

pub(super) fn act_archive(act: ActArtifact) -> String {
    format!("{ENGINE_CACHE}/tools/act-{ACT_VERSION}-{}.tgz", act.sha256)
}
