//! Small hosted-runner tools missing from the pinned slim Ubuntu image.
//! Kept in act's tool-cache volume, so the engine image stays unchanged and
//! completed generations survive across runs through the normal cache saver.

use super::{runner_tag, toolcache::TOOLCACHE_VOLUME};

const TOOLS_ID: &str = "pwsh-7.6.6-gh-2.102.0";
const BOOTSTRAP: &str = include_str!("runner_tools.sh");

pub(super) fn path_env() -> String {
    format!(
        "PATH=/opt/hostedtoolcache/bosn-runner-tools/{TOOLS_ID}/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    )
}

pub(super) fn prepare_script() -> String {
    let script = BOOTSTRAP
        .replace("@TOOLS_ID@", TOOLS_ID)
        .replace('\'', "'\\''");
    format!(
        "docker run --rm --platform linux/amd64 --volume {TOOLCACHE_VOLUME}:/opt/hostedtoolcache --entrypoint bash {} -ec '{script}'",
        runner_tag()
    )
}
