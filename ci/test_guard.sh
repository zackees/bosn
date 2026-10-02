#!/bin/sh
# Cargo runs every test binary through this guard (.cargo/config.toml;
# zackees/ci.yml GATE-005, bosn#361). bosn's tests start bosn daemons and touch
# bosn state roots, so a test binary (one under target/*/deps/) runs only in
# CI (CI=true: GitHub runners and act) or in the isolated bosn container,
# which sets BOSN_TEST_ISOLATED. Anything else (`cargo run`) runs as usual.
case "$1" in
  */deps/*)
    if [ "${CI:-}" != "true" ] && [ -z "${BOSN_TEST_ISOLATED:-}" ]; then
      echo "bosn tests never run on the developer host: run \`bosn run --task rust-test\`" >&2
      echo "(or the whole local gate: ci-lint local-gate run). See local-gate.toml." >&2
      exit 2
    fi
    ;;
esac
exec "$@"
