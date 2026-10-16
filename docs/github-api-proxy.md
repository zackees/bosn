# GitHub API proxies

Two proxies share one implementation (`crates/bosn-service/src/github_proxy.rs`:
credential resolution, nonce, masking, allowlist, ETag cache):

- [`github_api = "proxy"`](#read-only-github-api-proxy-github_api--proxy):
  read-only, for manifest tasks.
- [`bosn ci run --github-proxy`](#bosn-ci-run---github-proxy-650): a fake
  `GITHUB_TOKEN` that the daemon swaps for the real one, with scoped writes.

## Read-only GitHub API proxy (`github_api = "proxy"`)

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

### What the daemon does per run

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
forwarded (cache miss)`. Query strings are not logged.

### Allowlist

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

### ETag cache

`GET` 200 responses carrying an `ETag` are cached in the daemon (bounded:
4 MiB per body, 64 MiB and 1024 entries in total), keyed by a hash of the
credential, `Accept`, API version and URL, and shared by later tasks. A
repeat request is sent upstream with `If-None-Match`; GitHub answers `304`,
which does not count against the rate limit, and the client gets the cached
body with `x-bosn-cache: hit`. The cache is memory only and is lost when
the daemon restarts.

### Reachability, and why there is no relay

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

### act wiring (the task's side)

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

## `bosn ci run --github-proxy` (#650)

```sh
bosn ci run --workflow ci-pre.yml --job cache-janitor --trigger pr \
  --github-proxy --github-writes=dry-run --wait
```

Jobs that call the GitHub API with `GITHUB_TOKEN` (obs-studio's cache
janitor needs `GITHUB_TOKEN` and `GITHUB_REPOSITORY`) run with a stand-in
token. No real credential enters the engine or a container.

1. The daemon resolves the host credential as above (`gh auth token`, else
   the stored `github_token` secret, else anonymous). It lives only in daemon
   memory, and the run log masks it.
2. It mints a **fake token**, `bosn-proxy-<48 hex>` (192 random bits), and
   gives it to act as the secret `GITHUB_TOKEN`. That is both
   `secrets.GITHUB_TOKEN` and `github.token`, and the run log masks it. act
   sets `GITHUB_REPOSITORY` from the checkout's origin.
3. It starts the run's API proxy on loopback (the read-only proxy's code with
   a per-run policy) and a CONNECT front in front of it.
4. bosn's workflow overlay gives every job a first step that points
   `HTTPS_PROXY` at the run's relay and trusts the run's CA (below).
5. Every request to `api.github.com` must carry the fake token. The proxy
   swaps it for the real token, applies the allowlist and logs one line:
   `[github-api-proxy] DELETE /repos/o/r/actions/caches/42 -> 204 dry-run (not forwarded)`.

`--github-proxy` replaces `--github-token`; the daemon refuses both
together. It is also the `github_proxy`/`github_writes` arguments of the
`ci_run` MCP tool. It needs a Linux host and a checkout whose origin is a
GitHub repository.

### How `api.github.com` calls reach the proxy: `HTTPS_PROXY` + CONNECT interception

`gh`, ci-lint (`urllib` against a hard-coded `https://api.github.com`) and
octokit do not all honour `GITHUB_API_URL`, but every one of them honours
`HTTPS_PROXY`. So the proxy is an explicit HTTPS proxy:

- `CONNECT api.github.com:443` is **intercepted**. The daemon answers TLS
  itself with a leaf for `api.github.com` signed by a **per-run CA**. The CA
  is EC P-256, `CA:TRUE, pathlen:0`, valid for two days. Its key is dropped
  as soon as the leaf is signed, so nothing can mint another certificate
  under it, and a new CA is minted for every run. Each decrypted request is
  relayed to the run's API proxy.
- Every other `CONNECT host:port` is **tunnelled** byte for byte, so
  `HTTPS_PROXY` changes nothing else (git fetches, downloads, other APIs).
- The bootstrap step writes the CA (public) to `$RUNNER_TEMP` and sets these
  for every later step through `$GITHUB_ENV`:
  - `HTTPS_PROXY` and `https_proxy`;
  - `SSL_CERT_FILE`, `CURL_CA_BUNDLE`, `REQUESTS_CA_BUNDLE` and
    `GIT_SSL_CAINFO`, which name the system bundle plus the CA;
  - `NODE_EXTRA_CA_CERTS`, which names the CA alone;
  - `NO_PROXY` for loopback.

  The container's own trust store is never modified. The CA is trusted only
  by the run's job steps.

DNS or hosts remapping was rejected. act2 creates job containers through its
per-run Docker proxy, which exposes no hook for `--add-host` or DNS. It would
also need the same per-run CA, and it would capture every client of the host,
not only the ones that opt in through `HTTPS_PROXY`.

**Reaching the daemon from a job container.** Job containers run inside the
engine's nested Docker, and the engine sits on the host's bridge network.
Hosts commonly drop container-to-host traffic: on the development host every
connection from a bridge container to the host, gateway or LAN address times
out. So the daemon never listens for containers. Every connection is opened
from the host to the engine (`crates/bosn-service/src/github_relay.rs`):

- In the engine, busybox `nc -lk -e` runs two listeners per run, so no new
  binary enters the engine:
  - the **job port** (`42001 + 2·slot`), which jobs reach at their default
    gateway (the engine) and use as `HTTPS_PROXY`;
  - the **channel port** (`42000 + 2·slot`), bound to the engine's bridge
    address and dialled only by the daemon.
- The daemon keeps eight idle channels open. Each starts with a per-run
  relay nonce (sent over `docker exec` stdin, never argv). A channel without
  the nonce is closed.
- A job connection claims an idle channel and splices its socket to it.
  The claim is an atomic `mv` of the channel's marker; the splice is a FIFO
  pair. The daemon serves the connection and dials a fresh channel. A job
  connection that finds no idle channel waits up to 30 s.
- The relay starts after the run's scope is admitted and stops before the
  scope closes. act learns the job port as `BOSN_GITHUB_PROXY_PORT`.

The relay only moves bytes: TLS, the fake token, the allowlist and the real
credential stay in the daemon.

### The fake token

`bosn-proxy-` plus 48 lowercase hex digits. Real GitHub tokens are `ghp_`,
`gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_` or 40 hex digits, so the prefix
alone tells them apart, and the proxy never needs to look a token up. The
proxy accepts `Authorization: Bearer <fake>` or `token <fake>`, compared in
constant time, for this run's token only:

- a real token is answered `401` ("foreign token refused");
- another run's fake token is answered `401` ("belongs to another bosn run");
- no token is answered `401`.

A container never holds a real credential, so it cannot present one.

### Write scoping: `--github-writes=off|dry-run|on`

Reads are the read-only allowlist above plus what the cache janitor needs,
scoped to the run's own repository (`owner/name`, case-insensitive):

- `GET /repos/{own}/actions/caches`, `/actions/cache/usage`, `/pulls` and
  `/pulls/{n}`;
- `POST /graphql` when the document is a query: no `mutation` or
  `subscription` anywhere in it. The check is lexical and conservative, and
  the body is limited to 64 KiB. The janitor reads PR state this way.

Writes are a closed route+method list, scoped to the run's own repository:

- `DELETE /repos/{own}/actions/caches/{id}` (numeric id);
- `DELETE /repos/{own}/actions/caches?key=...` (by key, optionally with
  `ref`).

`--github-writes` decides what happens to them:

| Mode | An allowlisted write |
| --- | --- |
| `off` (the default) | is refused with `403` |
| `dry-run` | is logged and answered `204` (`x-bosn-dry-run: 1`), and never sent |
| `on` | is sent with the real credential |

Every other method or route is refused with `403` whatever the mode,
including a cache delete in another repository.

### What never leaks

- The real token stays in daemon memory: not on disk, not in argv, not in an
  environment, the registry, a record or a log, and never in a container.
- The fake token travels like any run secret: in the docker client's
  environment (`exec --env GITHUB_TOKEN`) and act's `-s GITHUB_TOKEN`.
- The run log masks both tokens.
- The run record stores only `github_proxy: "<mode>"`, never a value.
- Proxy log lines carry the method, the path without its query, the status
  and the outcome (`forwarded`, `dry-run (not forwarded)`, `refused`). The
  run log's first proxy line names the credential source.
