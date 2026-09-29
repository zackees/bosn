# Task secrets (`github_token`)

Local act runs call the GitHub API. Anonymously that budget is 60 requests per
hour per public IP, shared by every act run, `gh` call and `curl` on the
machine, so a local CI loop stops after a handful of runs (#308). Bosn can
inject a daemon-owned `GITHUB_TOKEN` into the tasks that ask for it.

## Declare it (by name only)

```toml
[task.act-ci-linux]
stack = "clud_act"
cmd = "sh /workspace/ci/act_ci.sh test-linux-x64-unit"
secrets = ["github_token"]   # injected as GITHUB_TOKEN
```

`secrets` is a task key. The manifest never holds a value or a path. The only
known name is `github_token` (injected as `GITHUB_TOKEN`); any other name, a
duplicate, or a non-array is refused at parse time. A task that does not
declare it gets no `GITHUB_TOKEN`, even when the secret exists, because the
token is added per `docker container exec`, never to the stack container.
Secrets are not supported on macOS guest tasks.

## Provision it

```sh
# Recommended: a fine-grained token with no permissions (public read only).
bosn secret set github_token < token.txt
# Convenience, with a scope warning: runs `gh auth token` once.
bosn secret set github_token --from-gh
bosn secret status [--json]        # names and present/missing/refused only
bosn secret remove github_token
```

All take `--state-dir` (default: `BOSN_STATE_DIR`, then the platform state
directory, the same as the daemon). The value is stored at
`STATE_DIR/secrets/github_token`, written atomically with mode 0600 in a 0700
directory. At task time the daemon refuses, like `guest-ssh/id_ed25519`, a
secret that is a symlink, sits under a symlinked directory, is not a regular
file, or has any group/world permission bit. A refused secret fails the task
with a message that never contains the value. A missing secret does not fail
the task: it runs without `GITHUB_TOKEN` and logs a one-line warning.

## How it is injected

The daemon puts the value only into the Docker client's process environment
and runs `docker container exec --env GITHUB_TOKEN <container> sh -lc <cmd>`.
The bare `--env NAME` makes Docker copy the value from its own environment, so
it never appears in argv, `ps`, the registry/SQLite, session records, or job
metadata. `DockerEngine`'s `Debug` output prints environment names only.

For act, pass it on as a secret inside the task, for example
`act -s GITHUB_TOKEN ...` (act reads the value from its environment). clud's
`ci/act_ci.sh` already does this when `GITHUB_TOKEN` is set.

## Masking

Task stdout and stderr are masked in the daemon before they become job log
records, which is the single path that feeds `bosn job logs`/`wait`, the MCP
tools and every client. The masker replaces the value with `***`, keeps an
independent tail per stream so a value split across writes is still caught
and stdout/stderr halves cannot join, and only holds back a proper prefix of
the value. The job's final error/summary text (which can quote task output) is
masked with the same masker. The HTTP task stream (#306) and act job/step
events (#307) do not exist yet; they must reuse `secrets::SecretMasker` at the
point where output leaves the container.

## Preflight

If a task declares `github_token` and none is provisioned, or its command
looks like an act run (`act` as a word) and it declares nothing, the task log
starts with a warning about the 60/hour anonymous limit and the remedy
`bosn secret set github_token`. No quota request is made yet.

## Scope risk

Whatever token act receives becomes `secrets.GITHUB_TOKEN` for every step of
every workflow run locally. A `gh auth token` is usually a long-lived OAuth
token with `repo`, `workflow` and often `admin:org`: a workflow step that
comments, pushes tags, creates releases or calls `gh` would act for real with
your full rights. Use a fine-grained token with **no** permissions; it is
enough to lift the limit to 5,000 requests/hour. To use your `gh` login
without giving it to any container, declare `github_api = "proxy"` instead;
see [github-api-proxy.md](github-api-proxy.md).
