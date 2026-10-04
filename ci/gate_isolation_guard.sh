#!/bin/sh
# bosn's gate lanes must never run against a developer host
# (zackees/ci.yml#168, GATE-005).
#
# bosn's Rust suite starts daemons and touches state roots. On a developer host
# that can claim or wedge a real daemon's cache, which is exactly the failure
# that took down soldr's ~/.soldr root (zackees/soldr#3516).
#
# The lane in ci/local_gate.py runs inside bosn, where the gate image sets the
# marker. This guard refuses everything else.
#
# CI=true covers GitHub runners and act; BOSN_TEST_ISOLATED=1 is set only by
# the isolated bosn gate image.
if [ "${CI:-}" != "true" ] && [ "${BOSN_TEST_ISOLATED:-}" != "1" ]; then
    echo "bosn's gate lanes refuse to run on a developer host (zackees/ci.yml#168, GATE-005)." >&2
    echo "Run them isolated:" >&2
    echo "  uv run --no-project python ci/local_gate.py" >&2
    exit 97
fi
exec "$@"