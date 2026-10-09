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

/// Copy visible published entries; hidden save stages and control files stay
/// in the shared store instead of consuming every fresh engine's private disk.
pub(super) fn seed_toolcache_script() -> String {
    format!(
        "src={ENGINE_CACHE}/toolcache; mkdir -p \"$src\" && docker volume create {TOOLCACHE_VOLUME} >/dev/null && \
         for entry in \"$src\"/*; do \
           [ -e \"$entry\" ] || [ -L \"$entry\" ] || continue; \
           cp -a \"$entry\" {TOOLCACHE_MOUNT}/ || exit $?; \
         done"
    )
}

const OVERLAY_RECIPE: &str = include_str!("toolcache_overlay.sh");

fn overlay_recipe() -> String {
    OVERLAY_RECIPE
        .replace("@STORE@", &format!("{ENGINE_CACHE}/toolstore-v1"))
        .replace("@TARGET@", TOOLCACHE_MOUNT)
}

/// Freeze the exact preparation recipe independently of the generation ID.
pub(super) fn overlay_recipe_sha256() -> String {
    kernal_api::hash::sha256_bytes(overlay_recipe().as_bytes()).to_hex()
}

/// A typed generation is admitted by engine PID1 before this overlay is built.
/// The private upper is shared by jobs in this engine, never by fresh engines.
pub(super) fn prepare_toolcache_script(
    generation: Option<&bosn_registry::act::ActToolGenerationBinding>,
) -> Result<String, String> {
    let Some(generation) = generation else {
        return Ok(seed_toolcache_script());
    };
    generation.validate().map_err(|error| error.to_string())?;
    if generation.overlay_recipe_sha256 != overlay_recipe_sha256() {
        return Err("tool overlay recipe differs from frozen producer".into());
    }
    Ok(overlay_recipe().replace("@GENERATION@", &generation.id))
}

/// Save each completed install the machine-wide copy lacks: copied to a temp
/// directory on the same filesystem, then renamed into place with `mv -T`,
/// which fails rather than merge when another engine saved it first. A
/// sibling marker is copied last; an inner stamp travels inside the renamed
/// directory. Either way a reader never sees a half-saved install as
/// complete.
///
/// The store is shared by every repository, so only a pristine install is
/// published (#557): one with nothing newer than its completion marker. A job
/// that ran `pip install` into setup-python's interpreter, or rewrote a tool,
/// changed the install after it completed; publishing that would hand one
/// repository's packages to every other. Files are compared, not directories:
/// an interpreter writes bytecode caches (exempt) on import, which also
/// touches their parent directory. A job that only deleted files is not
/// detected.
pub(super) fn save_toolcache_script() -> String {
    format!(
        "src={TOOLCACHE_MOUNT}; dst={ENGINE_CACHE}/toolcache; [ -d \"$src\" ] || exit 0; mkdir -p \"$dst\" || exit $?; cd \"$src\" || exit $?; \
         new_stage() {{ tmp=$(mktemp -d \"$dst/.saving-XXXXXXXX\") || return $?; \
           mkdir -p \"$dst/${{dir%/*}}\" || {{ code=$?; rm -rf \"$tmp\"; return \"$code\"; }}; }}; \
         pristine() {{ changed=$(find \"$dir\" -mindepth 1 ! -type d -newer \"$1\" ! -path '*/__pycache__/*' | head -n 1); \
           [ -z \"$changed\" ] && return 0; \
           echo \"tool cache: not saving $dir: changed after it completed ($changed)\" >&2; return 1; }}; \
         for marker in */*/*.complete; do \
           [ -f \"$marker\" ] || continue; dir=${{marker%.complete}}; \
           [ -d \"$dir\" ] && [ ! -e \"$dst/$marker\" ] || continue; \
           pristine \"$marker\" || continue; \
           new_stage || exit $?; \
           cp -a \"$dir\" \"$tmp/install\" && mv -T \"$tmp/install\" \"$dst/$dir\" 2>/dev/null && \
             cp \"$marker\" \"$dst/$marker\"; \
           rm -rf \"$tmp\"; \
         done; \
         find . -mindepth 3 -type f -name .complete | while read -r stamp; do \
           dir=${{stamp%/.complete}}; dir=${{dir#./}}; \
           [ ! -e \"$dst/$dir\" ] || continue; \
           pristine \"$stamp\" || continue; \
           new_stage || exit $?; \
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
    /// Back-date `path` by `secs`, so a later write is unambiguously newer.
    fn age(path: &Path, secs: u64) {
        let when = std::time::SystemTime::now() - Duration::from_secs(secs);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    /// #557: the store is shared by every repository, so an install a job
    /// changed after it completed (a `pip install` into setup-python's
    /// interpreter, a rewritten tool) is never published.
    #[test]
    fn installs_changed_after_completion_are_not_published() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let (src, cache) = (tmp.path().join("volume"), tmp.path().join("cache"));
        // Sibling-marker installs: one changed after completion, one only
        // compiled bytecode on import.
        write(src.join("Python/3.12.9/x64/bin/python"), "py");
        write(src.join("Python/3.12.9/x64.complete"), "");
        age(&src.join("Python/3.12.9/x64.complete"), 60);
        write(
            src.join("Python/3.12.9/x64/lib/site-packages/requests/__init__.py"),
            "repo A's dependency",
        );
        write(src.join("Python/3.11.9/x64/lib/os.py"), "stdlib");
        write(src.join("Python/3.11.9/x64.complete"), "");
        age(&src.join("Python/3.11.9/x64/lib/os.py"), 120);
        age(&src.join("Python/3.11.9/x64.complete"), 60);
        write(
            src.join("Python/3.11.9/x64/lib/__pycache__/os.pyc"),
            "bytecode",
        );
        // An inner-stamped install whose tool was rewritten after its stamp.
        let tools = "bosn-runner-tools/fixture";
        write(src.join(tools).join("bin/gh"), "pinned gh");
        write(src.join(tools).join(".complete"), "");
        age(&src.join(tools).join(".complete"), 60);
        write(src.join(tools).join("bin/gh"), "rewritten by a job");

        save(&src, &cache);
        let saved = cache.join("toolcache");
        assert!(
            !saved.join("Python/3.12.9").exists()
                && !saved.join("Python/3.12.9/x64.complete").exists(),
            "a changed install is not published"
        );
        assert!(
            !saved.join(tools).exists(),
            "a rewritten tool is not published"
        );
        assert!(
            saved.join("Python/3.11.9/x64.complete").exists(),
            "bytecode written on import does not block publication"
        );
        no_leftovers(&saved);
    }

    #[test]
    fn seeding_does_not_copy_unfinished_publication_stages() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let cache = tmp.path().join("cache");
        let target = tmp.path().join("private");
        write(cache.join("toolcache/Python/3.11/x64/bin/python"), "warm");
        write(cache.join("toolcache/Python/3.11/x64.complete"), "");
        write(
            cache.join("toolcache/.saving-collision/install/partial"),
            "unfinished",
        );
        std::fs::create_dir_all(&target).unwrap();
        let script = seed_toolcache_script()
            .replace(ENGINE_CACHE, &cache.to_string_lossy())
            .replace(TOOLCACHE_MOUNT, &target.to_string_lossy())
            .replace("docker volume create act-toolcache >/dev/null", "true");
        let result = std::process::Command::new("sh")
            .args(["-ec", &script])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(target.join("Python/3.11/x64/bin/python")).unwrap(),
            "warm"
        );
        assert!(
            !target.join(".saving-collision").exists(),
            "a fresh engine must not duplicate an unfinished cache save"
        );
    }

    #[cfg(unix)]
    const CONCURRENT_SAVE: &str = r##"
import json, os, pathlib, signal, subprocess, sys, tempfile, time
with tempfile.TemporaryDirectory() as directory:
    root = pathlib.Path(directory)
    cache = root / 'cache'
    cache.mkdir()
    tools = root / 'bin'
    tools.mkdir()
    spy = tools / 'cp'
    spy.write_text("""#!/usr/bin/env python3
import os, pathlib, sys, time
if sys.argv[-1].endswith('/install'):
    pathlib.Path(os.environ['BOSN_STAGE_FILE']).write_text(str(pathlib.Path(sys.argv[-1]).parent))
    deadline = time.monotonic() + 10
    while not pathlib.Path(os.environ['BOSN_STAGE_RELEASE']).exists():
        assert time.monotonic() < deadline, 'stage release timed out'
        time.sleep(0.01)
os.execv('/bin/cp', ['cp', *sys.argv[1:]])
""")
    spy.chmod(0o700)
    children = []
    stages = []
    release = root / 'release'
    try:
        for index, tool in enumerate(['Python', 'node']):
            source = root / ('source-' + str(index))
            install = source / tool / '1' / 'x64'
            install.mkdir(parents=True)
            (install / 'payload').write_text(tool)
            (install.parent / 'x64.complete').write_text('')
            stage = root / ('stage-' + str(index))
            # Separate engines can have the same PID. Replay that collision
            # deterministically; mktemp-based publication has no PID token.
            script = json.loads(sys.argv[1]).replace(sys.argv[2], str(cache)).replace(sys.argv[3], str(source)).replace('$$', '12345')
            child = subprocess.Popen(['sh', '-ec', script],
                env={**os.environ, 'PATH':str(tools)+':'+os.environ['PATH'],
                     'BOSN_STAGE_FILE':str(stage), 'BOSN_STAGE_RELEASE':str(release)},
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True)
            children.append(child)
            deadline = time.monotonic() + 5
            while not stage.exists():
                assert child.poll() is None, child.communicate(timeout=5)
                assert time.monotonic() < deadline, 'copy never reached publication stage'
                time.sleep(0.01)
            stages.append(stage.read_text())
        assert stages[0] != stages[1], 'different engines reused one publication stage'
        release.touch()
        for child in children:
            output, errors = child.communicate(timeout=10)
            assert child.returncode == 0, (child.returncode, output, errors)
        for tool in ['Python', 'node']:
            assert (cache / 'toolcache' / tool / '1' / 'x64' / 'payload').read_text() == tool
        assert not list((cache / 'toolcache').glob('.saving*'))
    finally:
        for child in children:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.communicate(timeout=5)
"##;

    #[cfg(unix)]
    #[test]
    fn concurrent_engines_with_equal_pids_publish_through_distinct_stages() {
        let result = std::process::Command::new("python3")
            .args([
                "-c",
                CONCURRENT_SAVE,
                &serde_json::to_string(&save_toolcache_script()).unwrap(),
                ENGINE_CACHE,
                TOOLCACHE_MOUNT,
            ])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    #[test]
    #[ignore = "requires the isolated bosn-456-live-v2 Docker engine"]
    fn real_engine_shell_rehydrates_completed_tools_without_hidden_stages() {
        assert!(
            std::env::var("DOCKER_HOST")
                .unwrap()
                .contains("bosn-456-live-v2-engine")
        );
        let directory = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let cidfile = directory.path().join("container-id");
        kernal_api::async_engine::RuntimeBuilder::multi_thread()
            .enable_all().build().unwrap().run(async {
                use super::super::{DockerActBackend, CONTROL_DEADLINE, engine_image, owned};
                let backend = DockerActBackend::default();
                let target = "/var/lib/docker/rehydrated";
                let seed = seed_toolcache_script()
                    .replace("docker volume create act-toolcache >/dev/null", "true")
                    .replace(TOOLCACHE_MOUNT, target);
                let source = format!("{TOOLCACHE_MOUNT}/Python/1/x64");
                let inner = format!("{TOOLCACHE_MOUNT}/soldr-syslib/linux-x64/zstd/1/slug");
                let script = format!(
                    "mkdir -p '{source}' '{inner}' '{target}'; \
                     printf warm > '{source}/payload'; printf complete > '{source}.complete'; \
                     printf inner > '{inner}/payload'; printf complete > '{inner}/.complete'; \
                     {}; \
                     mkdir -p {ENGINE_CACHE}/toolcache/.saving-orphan/install; \
                     printf unfinished > {ENGINE_CACHE}/toolcache/.saving-orphan/install/partial; \
                     {seed}; \
                     test \"$(cat '{target}/Python/1/x64/payload')\" = warm; \
                     test -f '{target}/Python/1/x64.complete'; \
                     test \"$(cat '{target}/soldr-syslib/linux-x64/zstd/1/slug/payload')\" = inner; \
                     test -f '{target}/soldr-syslib/linux-x64/zstd/1/slug/.complete'; \
                     test ! -e '{target}/.saving-orphan'; printf verified",
                    save_toolcache_script(),
                );
                let nonce = crate::ci::new_uuid().await.unwrap();
                let image = engine_image();
                let cidfile_text = cidfile.to_str().unwrap();
                let result = backend.checked("real tool cache copy", owned(&[
                    "run", "--rm", "--cidfile", cidfile_text, "--name", &format!("bosn-toolcache-proof-{nonce}"),
                    "--label", &format!("io.bosn.test.toolcache={nonce}"), "--pull", "never",
                    "--network", "none", "--read-only", "--cap-drop", "ALL", "--memory", "128m", "--cpus", "1",
                    "--tmpfs", ENGINE_CACHE, "--tmpfs", "/var/lib/docker", "--entrypoint", "sh", &image, "-ec", &script,
                ]), CONTROL_DEADLINE).await;
                // Reconcile the exact created ID even if run timed out. This
                // test never names a user container or mounts a real cache.
                let id = std::fs::read_to_string(&cidfile).unwrap();
                let id = id.trim();
                assert!(id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()));
                let inspect = backend.run(owned(&["container", "inspect", id]), CONTROL_DEADLINE).await.unwrap();
                if inspect.ok() {
                    backend.checked("tool cache proof cleanup", owned(&["rm", "-f", id]), CONTROL_DEADLINE).await.unwrap();
                }
                let absent = backend.run(owned(&["container", "inspect", id]), CONTROL_DEADLINE).await.unwrap();
                assert!(!absent.ok() && String::from_utf8_lossy(&absent.stderr).to_ascii_lowercase().contains("no such container"), "helper absence unproven: {id}");
                assert_eq!(result.unwrap(), "verified");
            });
    }
}
