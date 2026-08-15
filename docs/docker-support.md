<!-- GENERATED FILE -- DO NOT EDIT BY HAND.
     Regenerate with `uv run python ci/gen_docker_support.py`.
     Source of truth: src/bosn/frontdoor.py (the VERBS table). -->

# bosn-docker supported verbs

This is the full `bosn-docker` verb catalog, generated from the same `frontdoor.supported()` payload that `bosn-docker --supported --json` prints. Every verb bosn recognizes falls into exactly one of three categories.

Schema version: `1` (the shape of the `--supported --json` payload this doc reflects).

## Governed

| Verb | Summary |
| --- | --- |
| `init` | translate compose.yaml into a starting bosn.toml manifest |
| `compose` | managed Compose subset (up/down/logs/ps); every resource labeled and leased |

## Forwarded

| Verb | Summary |
| --- | --- |
| `version` | print client and engine version info (forwarded, read-only) |
| `info` | print engine-wide diagnostic info (forwarded, read-only) |
| `login` | authenticate to a registry (forwarded; touches local credential store only) |
| `logout` | clear registry credentials (forwarded; touches local credential store only) |

## Refused

| Verb | Summary | Remedy |
| --- | --- | --- |
| `run` | create and start an ad-hoc container | resource-creating; use `bosn run` for a manifest-declared stack, or `bosn-docker compose up` for a multi-service project |
| `create` | create a container without starting it | resource-creating; use `bosn ensure` to pre-warm a manifest-declared stack |
| `build` | build an image from a Dockerfile | resource-creating; declare the build in bosn.toml and use `bosn ensure`/`bosn run`, which key the built image to a spec digest and track it |
| `exec` | run a command in a running container | targets a raw container name outside bosn's registry; use `bosn shell` or `bosn run` against a manifest-declared stack |
| `start` | start a stopped container | mutates container lifecycle state outside the registry; use `bosn run` or `bosn ensure` to bring up a managed stack |
| `stop` | stop a running container | mutates container lifecycle state outside lease/GC bookkeeping; use `bosn done` to mark a workspace finished, or `bosn gc` to reclaim collectable resources |
| `restart` | restart a container | mutates container lifecycle state outside the registry; use `bosn run` or `bosn ensure` to bring up a managed stack fresh |
| `kill` | send a signal to a running container | mutates container lifecycle state outside the registry; use `bosn cancel` for a daemon-owned job, or `bosn gc` to reclaim collectable resources |
| `rm` | remove a container | deletes outside lease/GC bookkeeping; use `bosn gc` to reclaim managed resources safely |
| `rmi` | remove an image | deletes outside lease/GC bookkeeping; use `bosn gc` to reclaim managed resources safely |
| `pull` | download an image from a registry | creates a local image copy outside the registry; declare the image in bosn.toml and let `bosn ensure` pull it tracked |
| `push` | upload an image to a registry | not part of the managed subset; use the real `docker` binary directly if you accept the resources it creates will be unmanaged, or add it to bosn.toml so bosn can create it tracked |
| `tag` | create a new tag pointing at an existing image | creates a new local image reference outside the registry; not part of the managed subset; use the real `docker` binary directly if you accept the resources it creates will be unmanaged, or add it to bosn.toml so bosn can create it tracked |
| `cp` | copy files into or out of a container | targets a raw container name outside bosn's registry; use `bosn shell` to reach a managed container's filesystem |
| `network` | manage networks (create/rm/connect/...) | networks are declared implicitly by a stack and labeled by bosn; use `bosn status` to inspect them, `bosn gc` to reclaim them |
| `volume` | manage volumes (create/rm/prune/...) | volumes are declared in bosn.toml and labeled by bosn; use `bosn status` to inspect them, `bosn gc` to reclaim them |
| `image` | manage images (build/rm/prune/...) | images are generation-keyed and managed by bosn; use `bosn status`/`bosn gc` instead of the raw image subcommands |
| `container` | manage containers (create/rm/prune/...) | containers are declared in bosn.toml and labeled by bosn; use `bosn status`/`bosn gc` instead of the raw container subcommands |
| `system` | manage or inspect the engine (df/prune/events/...) | `system prune` in particular deletes resources bosn did not choose to reclaim; use `bosn gc` for a governed equivalent |
| `save` | export an image to a tar archive | not part of the managed subset; use the real `docker` binary directly if you accept the resources it creates will be unmanaged, or add it to bosn.toml so bosn can create it tracked |
| `load` | import an image from a tar archive | creates a local image outside the registry; not part of the managed subset; use the real `docker` binary directly if you accept the resources it creates will be unmanaged, or add it to bosn.toml so bosn can create it tracked |
| `export` | export a container's filesystem to a tar archive | not part of the managed subset; use the real `docker` binary directly if you accept the resources it creates will be unmanaged, or add it to bosn.toml so bosn can create it tracked |
| `import` | create an image from a tarball | creates a local image outside the registry; not part of the managed subset; use the real `docker` binary directly if you accept the resources it creates will be unmanaged, or add it to bosn.toml so bosn can create it tracked |
| `commit` | create a new image from a container's changes | creates a local image outside the registry; not part of the managed subset; use the real `docker` binary directly if you accept the resources it creates will be unmanaged, or add it to bosn.toml so bosn can create it tracked |
| `rename` | rename a container | mutates a container's identity outside the registry's naming; not part of the managed subset; use the real `docker` binary directly if you accept the resources it creates will be unmanaged, or add it to bosn.toml so bosn can create it tracked |
| `attach` | attach local streams to a running container | targets a raw container name outside bosn's registry; use `bosn attach` for a daemon-owned job |
| `ps` | list containers | lists raw engine state instead of bosn's managed view; use `bosn status` or `bosn tasks` for governed introspection |
| `logs` | fetch a container's logs | targets a raw container name outside bosn's registry; use `bosn-docker compose logs` for a managed stack, or `bosn attach` for a daemon-owned job |
| `inspect` | show low-level details of an object | targets a raw object name outside bosn's registry; use `bosn status` for governed introspection |
| `top` | list processes running in a container | targets a raw container name outside bosn's registry; use `bosn jobs` for governed introspection |
| `stats` | stream resource usage statistics | targets raw container names outside bosn's registry; use `bosn status` for governed introspection |
| `events` | stream real-time engine events | surfaces raw engine activity outside bosn's registry; use `bosn status` for governed introspection |
| `port` | list a container's published ports | targets a raw container name outside bosn's registry; use `bosn status` for governed introspection |
| `diff` | list changed files in a container's filesystem | targets a raw container name outside bosn's registry; use `bosn shell` to inspect a managed container directly |
| `wait` | block until a container stops, then print its exit code | targets a raw container name outside bosn's registry; use `bosn jobs`/`bosn attach` to wait on a daemon-owned job |
| `search` | search Docker Hub for images | not part of the managed subset; use the real `docker` binary directly if you accept the resources it creates will be unmanaged, or add it to bosn.toml so bosn can create it tracked |

