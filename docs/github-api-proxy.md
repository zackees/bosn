# Read-only GitHub API proxy (`github_api = "proxy"`)

A task that needs authenticated GitHub API *reads* (act runs hit the
60/hour anonymous limit, #308) can use the host's `gh` login without the
token ever entering a container.

```toml
[task.act-ci-linux]
stack = "clud_act"
cmd = "sh /workspace/ci/act_ci.sh test-linux-x64-unit"
github_api = "proxy"   # the only accepted value; the manifest holds no URL or token
```

A task that does not declare it gets nothing. It is refused on macOS guest
tasks.

## What the daemon does per run

1. Resolves a credential, in order: a fresh `gh auth token` run on the host
   by the daemon (stdout captured through a pipe, never logged), else the
   stored `github_token` secret (`bosn secret set`), else anonymous. It is
   held in daemon memory only: never written to disk, the registry, argv, an
   environment, or a log. It is also added to the task's output masker.
2. Binds a proxy on `127.0.0.1:<ephemeral port>` for the life of the task and
   injects exactly one variable into the `docker exec`:
   `GITHUB_API_URL=http://127.0.0.1:<port>/<nonce>` (192-bit random nonce;
   requests without it get 404). The nonce is masked as `***` in job logs.
3. Stops the listener when the task ends.

The first task log line names the credential source (`gh auth token`,
stored secret, or anonymous) without its value, and each proxied request
logs one line: `[github-api-proxy] GET /repos/o/r/releases/latest -> 200
(cache miss)`. Query strings are not logged.

## Allowlist

Only `GET` and `HEAD` are forwarded, to `https://api.github.com` only:

- `/rate_limit`
- `/repos/{owner}/{repo}`
- `/repos/{owner}/{repo}/releases/...`, `/tags`, `/branches/...`,
  `/commits/...`, `/contents/...`, `/zipball/...`, `/tarball/...`
- `/repos/{owner}/{repo}/git/{ref,refs,matching-refs,trees,blobs,commits,tags}/...`

Everything else, including every write method, `/user`, `/graphql`, org
endpoints, and repo `actions`/`hooks`/`keys`/`issues`/`pulls`, is refused
with `403` and a JSON body whose `message` starts
`bosn github api proxy:`. Non-canonical paths (`..`, `//`, encoded `.` `/`
`\`) are refused. The client's `Authorization` header is discarded and
replaced; request bodies are never forwarded. Redirects are not followed:
a `302` (zipball, release asset downloads) is relayed with its `Location`,
and the client fetches that signed URL itself. Relayed response headers are
a fixed set (content type, ETag, Last-Modified, Link, Location, rate-limit
headers); `Link` pagination URLs are rewritten to point back through the
proxy.

Note that the credential is usually your full-rights `gh` OAuth token, so
allowlisted reads can see private repositories you can see. Writes cannot
pass the proxy.

## ETag cache

`GET` 200 responses carrying an `ETag` are cached in the daemon (bounded:
4 MiB per body, 64 MiB and 1024 entries in total), keyed by a hash of the
credential, `Accept`, API version and URL, and shared by later tasks. A
repeat request is sent upstream with `If-None-Match`; GitHub answers `304`,
which does not count against the rate limit, and the client gets the cached
body with `x-bosn-cache: hit`. The cache is memory only and is lost when
the daemon restarts.

## Reachability, and why there is no relay

act 0.2.x starts job containers with `--network host` (its `--network`
default). They are siblings on the host engine, and `127.0.0.1` inside them
is the host's loopback, where the proxy listens. So no in-container relay is
needed: a relay would add a second listener (and a binary to inject into
every stack) without making anything reachable that is not already. The
stack container itself is on a bridge network and cannot reach the proxy;
act does not need it to, it only forwards the URL to its jobs. Loopback
binding keeps the proxy off the LAN and off bridge-networked containers;
the nonce keeps other host processes that do not hold it out.

A job or service container started with a non-host network cannot reach the
proxy. That is a known limit.

## act wiring (the task's side)

act computes `github.api_url` and each step's `GITHUB_API_URL` from its
`--env GITHUB_API_URL=...` value. The bare form `--env GITHUB_API_URL`
sets it to the empty string in act 0.2.88 (despite its help text), so pass
the value:

```sh
[ -z "${GITHUB_API_URL:-}" ] || set -- "$@" --env "GITHUB_API_URL=$GITHUB_API_URL"
```

Only clients that honour `GITHUB_API_URL` (for example `@actions/github`'s
Octokit, `actions/github-script`, or `curl "$GITHUB_API_URL/..."`) use the
proxy. Actions that hard-code `https://api.github.com` bypass it and stay
anonymous. As of this writing that includes `zackees/setup-soldr` and
`@actions/tool-cache`'s `getManifestFromRepo` (used by `actions/setup-python`
when the Python version is not in the runner's tool cache; it falls back to
`raw.githubusercontent.com` on failure).

Do not pass a placeholder `GITHUB_TOKEN` to the workflow while such
hard-coded clients run: they would send it to the real API and fail with
401, where anonymous access would still succeed. The proxy ignores whatever
`Authorization` a client sends, so a proxy-aware client needs no token.
