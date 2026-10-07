//! Shared digest-verified act installation for bootstrap and readiness.

use super::{
    ACT_VERSION, ActArtifact, CONTROL_DEADLINE, DockerActBackend, ENGINE_CACHE, ENGINE_WORK,
};

impl DockerActBackend {
    /// Query the same digest-verified executable used for workflow execution.
    /// Bootstrap and offline readiness both require this evidence contract.
    pub(super) async fn verify_installed_act(
        &self,
        engine: &str,
        version: &str,
    ) -> Result<(), String> {
        if !version.ends_with(ACT_VERSION) {
            return Err(format!(
                "installed act reports {version:?}, expected {ACT_VERSION}"
            ));
        }
        self.verify_act_capabilities(engine, false).await
    }

    pub(super) async fn verify_act_capabilities(
        &self,
        engine: &str,
        selected_outputs: bool,
    ) -> Result<(), String> {
        let document = self
            .checked(
                "act execution capabilities",
                Self::exec(engine, &format!("{ENGINE_WORK}/bin/act --ci-capabilities")),
                CONTROL_DEADLINE,
            )
            .await?;
        super::act_capabilities::validate(&document, ACT_VERSION, selected_outputs)
    }
}

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
