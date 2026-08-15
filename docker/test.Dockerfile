FROM ghcr.io/astral-sh/uv:0.12.3@sha256:2d890623d310b57771ce840f0da5eed5fc6d657da05ffaa45d82797b53fa3abc AS uv
FROM python:3.13-slim-bookworm@sha256:00faa2debb87529f9f0764e9491d8ba400a3678976616c3bd7cb193745ac20d1

COPY --from=uv /uv /uvx /bin/

RUN apt-get update \
    && apt-get install --yes --no-install-recommends git libatomic1 \
    && rm -rf /var/lib/apt/lists/*

ENV UV_PROJECT_ENVIRONMENT=/venv \
    UV_CACHE_DIR=/root/.cache/uv \
    UV_LINK_MODE=copy \
    RUFF_CACHE_DIR=/root/.cache/ruff \
    PYRIGHT_PYTHON_CACHE_DIR=/root/.cache/pyright-python \
    PYTHONDONTWRITEBYTECODE=1

WORKDIR /repo
