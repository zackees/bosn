//! act's tool cache, seeded from and saved back to the machine-wide cache.
//!
//! act mounts its `act-toolcache` volume at `/opt/hostedtoolcache` in every
//! job container. The engine is fresh per run, so that volume is seeded from
//! the machine-wide copy before act starts and every newly completed install
//! is saved back after it ends. Two completion conventions are recognised:
//!
//! - `actions/tool-cache`: `<tool>/<version>/<arch>` with a sibling
//!   `<arch>.complete` marker (setup-python, setup-uv, setup-node, ...);
//! - an install directory (at least two levels deep) holding its own
//!   `.complete` stamp, written last
//!   (soldr's syslib store, which setup-soldr keeps under
//!   `soldr-syslib/<platform>/<lib>/<version>/<slug>`, setup-soldr#553).

use super::ENGINE_CACHE;

pub(super) const TOOLCACHE_VOLUME: &str = "act-toolcache";
pub(super) const TOOLCACHE_MOUNT: &str = "/var/lib/docker/volumes/act-toolcache/_data";

pub(super) fn seed_toolcache_script() -> String {
    format!(
        "mkdir -p {ENGINE_CACHE}/toolcache && docker volume create {TOOLCACHE_VOLUME} >/dev/null && \
         cp -a {ENGINE_CACHE}/toolcache/. {TOOLCACHE_MOUNT}/"
    )
}

/// Save each completed install the machine-wide copy lacks: copied to a temp
/// directory on the same filesystem, then renamed into place with `mv -T`,
/// which fails rather than merge when another engine saved it first. A
/// sibling marker is copied last; an inner stamp travels inside the renamed
/// directory. Either way a reader never sees a half-saved install as
/// complete.
pub(super) fn save_toolcache_script() -> String {
    format!(
        "src={TOOLCACHE_MOUNT}; dst={ENGINE_CACHE}/toolcache; [ -d \"$src\" ] || exit 0; cd \"$src\"; \
         for marker in */*/*.complete; do \
           [ -f \"$marker\" ] || continue; dir=${{marker%.complete}}; \
           [ -d \"$dir\" ] && [ ! -e \"$dst/$marker\" ] || continue; \
           tmp=\"$dst/.saving-$$\"; rm -rf \"$tmp\"; mkdir -p \"$tmp\" \"$dst/${{dir%/*}}\"; \
           cp -a \"$dir\" \"$tmp/install\" && mv -T \"$tmp/install\" \"$dst/$dir\" 2>/dev/null && \
             cp \"$marker\" \"$dst/$marker\"; \
           rm -rf \"$tmp\"; \
         done; \
         find . -mindepth 3 -type f -name .complete | while read -r stamp; do \
           dir=${{stamp%/.complete}}; dir=${{dir#./}}; \
           [ ! -e \"$dst/$dir\" ] || continue; \
           tmp=\"$dst/.saving-$$\"; rm -rf \"$tmp\"; mkdir -p \"$tmp\" \"$dst/${{dir%/*}}\"; \
           cp -a \"$dir\" \"$tmp/install\" && mv -T \"$tmp/install\" \"$dst/$dir\" 2>/dev/null; \
           rm -rf \"$tmp\"; \
         done"
    )
}

#[cfg(test)]
mod tests {
    use std::{path::Path, time::Duration};

    use super::*;

    fn write(path: std::path::PathBuf, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// Run the save script for real, from `src` (the act volume) into `cache`.
    fn save(src: &Path, cache: &Path) {
        let script = save_toolcache_script()
            .replace(TOOLCACHE_MOUNT, &src.to_string_lossy())
            .replace(ENGINE_CACHE, &cache.to_string_lossy());
        let out = kernal_api::run_bounded_command(
            kernal_api::SpawnSpec::new("sh")
                .arg("-ec")
                .arg(&script)
                .stdin(kernal_api::StreamMode::Null)
                .stdout(kernal_api::StreamMode::Piped)
                .stderr(kernal_api::StreamMode::Piped),
            Duration::from_secs(30),
            1 << 16,
        )
        .unwrap();
        assert_eq!(
            out.exit.raw_code(),
            0,
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn no_leftovers(saved: &Path) {
        let leftovers = std::fs::read_dir(saved)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(".saving"))
            .count();
        assert_eq!(leftovers, 0, "no temp directory is left behind");
    }

    #[test]
    fn saving_the_tool_cache_copies_only_complete_installs_once() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let (src, cache) = (tmp.path().join("volume"), tmp.path().join("cache"));
        write(src.join("Python/3.11.17/x64/bin/python"), "py");
        write(src.join("Python/3.11.17/x64.complete"), "");
        write(src.join("uv/0.12.19/x86_64/uv"), "half-written: no marker");
        write(cache.join("toolcache/node/24/x64/kept"), "existing");
        write(cache.join("toolcache/node/24/x64.complete"), "");
        write(
            src.join("node/24/x64/new"),
            "must not replace the saved one",
        );
        write(src.join("node/24/x64.complete"), "");
        save(&src, &cache);
        let saved = cache.join("toolcache");
        assert_eq!(
            std::fs::read_to_string(saved.join("Python/3.11.17/x64/bin/python")).unwrap(),
            "py"
        );
        assert!(saved.join("Python/3.11.17/x64.complete").exists());
        assert!(
            !saved.join("uv").exists(),
            "an install without its marker is incomplete"
        );
        assert!(saved.join("node/24/x64/kept").exists());
        assert!(
            !saved.join("node/24/x64/new").exists(),
            "a saved install is never replaced"
        );
        save(&src, &cache);
        no_leftovers(&saved);
    }

    /// setup-soldr#553 keeps soldr's syslib store in the tool cache; soldr
    /// stamps each install with an inner `.complete`, written last.
    #[test]
    fn saving_the_tool_cache_keeps_inner_stamped_installs() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let (src, cache) = (tmp.path().join("volume"), tmp.path().join("cache"));
        let store = "soldr-syslib/linux-x64";
        write(
            src.join(store)
                .join("zstd/1.5.7/linux-x64-gnu/package/lib/libzstd.a"),
            "z",
        );
        write(
            src.join(store).join("zstd/1.5.7/linux-x64-gnu/.complete"),
            "zstd 1.5.7",
        );
        write(
            src.join(store)
                .join("cmake/4.3.4/linux-x64-gnu/package/bin/cmake"),
            "no stamp",
        );
        write(
            cache
                .join("toolcache")
                .join(store)
                .join("ninja/1.13.2/linux-x64-gnu/.complete"),
            "",
        );
        write(
            src.join(store).join("ninja/1.13.2/linux-x64-gnu/new"),
            "must not replace",
        );
        write(
            src.join(store).join("ninja/1.13.2/linux-x64-gnu/.complete"),
            "",
        );
        save(&src, &cache);
        let saved = cache.join("toolcache").join(store);
        assert_eq!(
            std::fs::read_to_string(saved.join("zstd/1.5.7/linux-x64-gnu/package/lib/libzstd.a"))
                .unwrap(),
            "z"
        );
        assert!(saved.join("zstd/1.5.7/linux-x64-gnu/.complete").exists());
        assert!(
            !saved.join("cmake").exists(),
            "an install without its stamp is incomplete"
        );
        assert!(
            !saved.join("ninja/1.13.2/linux-x64-gnu/new").exists(),
            "a saved install is never replaced"
        );
        save(&src, &cache);
        no_leftovers(&cache.join("toolcache"));
    }
}
