//! One exclusive lease spans import, host validation and route publication.
use super::{CacheMigrationSession, DockerActBackend, ENGINE_CACHE, ENGINE_WORK, owned};
use crate::ci::{cache_cohort::Namespace, cache_policy::CachePolicy};
use std::time::Duration;

pub(super) const BUSY: &str = "cache enrollment lease busy";

#[cfg(all(test, unix))]
#[path = "migration_session_live.rs"]
mod live;

impl DockerActBackend {
    /// The caller must separately exclude nonparticipating historical writers.
    /// This protocol holds the participating lease until stdin closes, abort,
    /// successful publication, or the independent 180-second remote timeout.
    /// Host validation must finish before sending `publish` and a typed route.
    /// Each command emits a JSON body followed by `bosn-migration-end:<exit>`.
    /// Callers must impose an earlier absolute deadline and output byte ceiling.
    pub async fn open_cache_migration(
        &self,
        engine: &str,
        namespace: &Namespace,
        policy: CachePolicy,
    ) -> Result<CacheMigrationSession, String> {
        let mut args = owned(&["exec", "-i", engine, "timeout", "180", "sh", "-c"]);
        args.push(protocol(namespace, policy));
        let session = self
            .docker
            .with_args(args)
            .spawn_interactive(Duration::from_secs(15))
            .await
            .map_err(|error| error.to_string())?;
        CacheMigrationSession::ready(session, namespace.clone(), policy).await
    }
}

fn protocol(namespace: &Namespace, policy: CachePolicy) -> String {
    // Every argument is fixed text or a validated hash/numeric policy value.
    let import = policy.import_args_for_quiescent_source(namespace).join(" ");
    let route = namespace.routing_record_path();
    format!(
        r#"set -eu
directory={ENGINE_CACHE}/actcache
[ ! -L "$directory" ] || exit 78
mkdir -p "$directory"
maintenance="$directory/.bosn-maintenance-v1.lock"
[ ! -L "$maintenance" ] && {{ [ ! -e "$maintenance" ] || [ -f "$maintenance" ]; }} || exit 78
exec 6>>"$maintenance"
flock -x -n 6 || {{ printf 'bosn-migration-busy\n'; exit 75; }}
lock="$directory/.legacy-migration.lock"
[ ! -L "$lock" ] && {{ [ ! -e "$lock" ] || [ -f "$lock" ]; }} || exit 78
exec 8>>"$lock"
flock -x -n 8 || {{ printf 'bosn-migration-busy\n'; exit 75; }}
printf 'bosn-migration-ready\n'
while IFS= read -r operation; do
    case "$operation" in
        bootstrap)
            source={source}
            [ ! -L "$source" ] || exit 78
            if [ -e "$source" ]; then
                [ -d "$source" ] || exit 78
                printf 'existing\nbosn-migration-end:0\n'
                continue
            fi
            bootstrap={ENGINE_WORK}/cache-bootstrap
            mkdir -p "$bootstrap/home" "$bootstrap/tmp"
            printf '%s\n' 'on: [push]' 'jobs:' '  bootstrap:' '    if: false' '    runs-on: ubuntu-latest' '    steps:' '      - run: true' >"$bootstrap/workflow.yml"
            code=0
            (cd "$bootstrap"; HOME="$bootstrap/home" XDG_CACHE_HOME="$bootstrap/home/.cache" XDG_CONFIG_HOME="$bootstrap/home/.config" TMPDIR="$bootstrap/tmp" \
                {ENGINE_WORK}/bin/act push -n -W workflow.yml --cache-server-path "$source" --cache-server-addr 127.0.0.1 --artifact-server-addr 127.0.0.1 \
                --artifact-server-path "$bootstrap/artifacts" --action-cache-path "$bootstrap/actions" --container-architecture linux/amd64 --platform ubuntu-latest={runner} --pull=false </dev/null) >&2 || code=$?
            printf 'initialized\nbosn-migration-end:%s\n' "$code"
            ;;
        destination)
            destination={destination}
            parent=${{destination%/*}}
            [ ! -L "$parent" ] && [ ! -L "$destination" ] || exit 78
            if [ ! -e "$destination" ]; then
                [ ! -e "$parent" ] || [ -d "$parent" ] || exit 78
                printf 'absent\nbosn-migration-end:0\n'
            else
                [ -d "$destination" ] || exit 78
                printf 'present\nbosn-migration-end:0\n'
            fi
            ;;
        import)
            code=0
            {ENGINE_WORK}/bin/act {import} || code=$?
            printf '\nbosn-migration-end:%s\n' "$code"
            ;;
        audit)
            code=0
            {ENGINE_WORK}/bin/act cache audit --cache-server-path {destination} || code=$?
            printf '\nbosn-migration-end:%s\n' "$code"
            ;;
        receipt)
            code=0
            {ENGINE_WORK}/bin/act cache import-receipt --cache-server-path {destination} || code=$?
            printf '\nbosn-migration-end:%s\n' "$code"
            ;;
        publish)
            IFS= read -r record || exit 78
            [ "${{#record}}" -le 4096 ] || exit 78
            route={route}
            routes=${{route%/*}}
            [ ! -L "$routes" ] && [ ! -L "$route" ] || exit 78
            mkdir -p "$routes"
            umask 077
            stage=$(mktemp "$routes/.publication.XXXXXXXX")
            trap 'rm -f "$stage"' EXIT
            printf '%s' "$record" >"$stage"
            sync -f "$stage"
            if [ ! -e "$route" ]; then ln "$stage" "$route"; fi
            [ -f "$route" ] && cmp -s "$stage" "$route" || exit 78
            sync -f "$routes"
            printf 'bosn-migration-published\n'
            exit 0
            ;;
        abort) exit 0 ;;
        *) exit 78 ;;
    esac
done
"#,
        destination = namespace.path(),
        source = namespace.legacy_path(),
        runner = crate::ci::engine::runner_tag()
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn exclusive_lease_spans_validation_and_publication_and_releases_on_disconnect() {
        let namespace = Namespace::parse("0123456789abcdef").unwrap();
        let policy = CachePolicy::default();
        let record =
            crate::ci::cache_routing::RoutingRecord::new(&namespace, policy, &"a".repeat(64))
                .unwrap();
        let script = r#"
import fcntl, pathlib, subprocess, sys, tempfile
with tempfile.TemporaryDirectory() as temporary:
    root = pathlib.Path(temporary)
    cache, work = root / 'cache', root / 'work'
    binary = work / 'bin' / 'act'
    binary.parent.mkdir(parents=True)
    binary.write_text('#!/bin/sh\nprintf "{}\\n"\n')
    binary.chmod(0o700)
    command = sys.argv[1].replace(sys.argv[2], str(cache)).replace(sys.argv[3], str(work))
    def start():
        child = subprocess.Popen(['timeout', '10', 'sh', '-c', command], stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        assert child.stdout.readline() == b'bosn-migration-ready\n'
        return child
    child = start()
    try:
        with (cache / 'actcache' / '.legacy-migration.lock').open('rb') as observer:
            def blocked():
                try: fcntl.flock(observer, fcntl.LOCK_SH | fcntl.LOCK_NB)
                except BlockingIOError: return
                raise AssertionError('legacy writer entered migration interval')
            maintenance_path = cache / 'actcache' / '.bosn-maintenance-v1.lock'
            with maintenance_path.open('ab') as maintenance:
                try: fcntl.flock(maintenance, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError: pass
                else: raise AssertionError('maintenance entered migration interval')
            blocked()
            for operation in [b'import\n', b'audit\n']:
                child.stdin.write(operation); child.stdin.flush()
                assert child.stdout.readline() == b'{}\n'
                assert child.stdout.readline() == b'\n'
                assert child.stdout.readline() == b'bosn-migration-end:0\n'
                blocked()
            route = pathlib.Path(sys.argv[4].replace(sys.argv[2], str(cache)))
            assert not route.exists()
            child.stdin.write(b'publish\n' + sys.argv[5].encode() + b'\n'); child.stdin.flush()
            assert child.stdout.readline() == b'bosn-migration-published\n'
            assert child.wait(timeout=5) == 0, child.stderr.read()
            assert route.read_text() == sys.argv[5]
            fcntl.flock(observer, fcntl.LOCK_SH | fcntl.LOCK_NB)
            fcntl.flock(observer, fcntl.LOCK_UN)
        child = start()
        child.stdin.close()
        assert child.wait(timeout=5) == 0
        with (cache / 'actcache' / '.legacy-migration.lock').open('rb') as observer:
            fcntl.flock(observer, fcntl.LOCK_EX | fcntl.LOCK_NB)
    finally:
        if child.poll() is None: child.kill(); child.wait(timeout=5)
"#;
        let output = std::process::Command::new("python3")
            .args([
                "-c",
                script,
                &protocol(&namespace, policy),
                ENGINE_CACHE,
                ENGINE_WORK,
                &namespace.routing_record_path(),
                &String::from_utf8(record.encode().unwrap()).unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
