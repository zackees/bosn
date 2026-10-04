//! Machine-volume exclusion for participating maintenance commands.
use super::{ENGINE_CACHE, ENGINE_WORK};

pub(super) fn command() -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        format!(
            "mkdir -p {ENGINE_CACHE}/actcache || exit $?; \
            exec 6>>{ENGINE_CACHE}/actcache/.bosn-maintenance-v1.lock || exit $?; \
            flock -x -n 6 || {{ echo 'machine cache maintenance busy' >&2; exit 75; }}; \
            exec {ENGINE_WORK}/bin/act \"$@\""
        ),
        "bosn-machine-maintenance-lease".into(),
    ]
}
