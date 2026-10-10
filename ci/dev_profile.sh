# Sourced by ./install, ./lint and ./test: the one place the dev/test wheel
# profile default is written (AGENTS.md, "Test install profile").
# BOSN_WHEEL_PROFILE is a uv cache key (pyproject.toml), so every `uv sync`
# and `uv run` of a dev checkout must see the same value. Before this file,
# ./install built a dev wheel and the next bare `uv run` in ./lint saw the
# key change and rebuilt bosn in release profile (about 5 minutes per gate,
# bosn#503), which ./test then reused.
export BOSN_WHEEL_PROFILE="${BOSN_WHEEL_PROFILE:-dev}"
