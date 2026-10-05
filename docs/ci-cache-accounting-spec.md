# CI cache and Docker footprint: living spec

This document records the verified behavior of Bosn's `act` engine and the
remaining work to make repeated CI runs fast without unbounded disk growth.
Update the **Current implementation** and **Evidence** sections in the same
change as each implementation step. The desired behavior below is a contract,
not a claim that it is already implemented. Related issue: #456.

## Survey and evidence (2026-10-03)

The read-only host audit in #456 measured ~859 GiB under `/var/lib/docker`.
`bosn-ci-cache-v1` was ~49 GiB, of which `actcache/` was ~41 GiB. Seventy-six
unattached `bosn-v-stack-*` or `bosn-v-machine-*` volumes were present; their
attachment state alone says nothing about safe removal. An ~18 GiB anonymous
volume sampled during the audit was attached to an active CI engine. The audit
did not establish an anonymous-volume leak.

The current code and tests give the following narrower evidence:

| Resource | Reuse between runs now | Accounting now | Expiration now |
|---|---|---|---|
| act release | Verified tar in shared `bosn-ci-cache-v1/tools/` | Included in total `runners cache` bytes | Whole-cache clear only |
| Runner image | Saved tar in shared `images/`, loaded into each fresh engine | Included in total cache bytes; each active engine also holds its own loaded layers | Engine removal deletes nested copy; shared tar lasts until whole-cache clear |
| Action checkouts | Shared `actions/` path passed to act | Included in total cache bytes | Whole-cache clear only |
| `actions/cache` archives | Shared `actcache/<repository hash>/` path passed to act | Included in total cache bytes, without per-repository breakdown | Whole-cache clear only; no age or size limit |
| `/opt/hostedtoolcache` | Seeded from shared `toolcache/`; completed installs copied back before teardown | Included in total cache bytes | Whole-cache clear only |
| Job container/image layers and job build cache | Fresh nested Docker engine per run; runner tar is loaded, other layers rebuilt or pulled | Nested bytes are not attributed per run in CI cache usage | `docker rm -f -v` removes engine and anonymous storage on successful cleanup |
| CI run records/source snapshots | Retained in daemon state, separate from Docker cache | `runners prune-cache --max-bytes` measures these files | Count, age and size pruning |
| Host Docker build cache and non-CI Bosn volumes | Outside the CI cache volume | Unmanaged census reports only unowned artifacts; owned resources are protected there | Separate conservative lifecycle commands; no aggregate owned-volume quota |

Evidence: `crates/bosn-service/src/ci/engine.rs` mounts the named volume,
constructs act cache paths, seeds and saves tool installs, saves the runner
tar, and removes engines with `rm -f -v`. `crates/bosn-service/src/ci/runtime/runners.rs`
measures the whole volume and prunes run records separately.
`crates/bosn-service/tests/ci_live.rs` contains an opt-in second-run
`actions/cache` restore proof; its module also describes leak checks.
`crates/bosn-service/src/ci/lifecycle.rs` has fault-path and restart
reconciliation tests, but those are not proof of a host-wide storage ceiling.
`docs/rust-unmanaged.md` documents the existing machine-wide *unmanaged*
census and its deliberate exclusion of resources owned by this registry.

## Required behavior

1. **Warm jobs.** Jobs in the same repository can reuse safe `actions/cache`
   entries, action checkouts and completed tool installs across runs and
   across fresh nested engines. Different repositories cannot read each
   other's `actions/cache` entries. Concurrent jobs cannot observe partially
   written cache entries. A newly created engine may be cold for its private
   image layers, but its reusable inputs must be hydrated from the shared
   store. Keep the private Docker socket boundary.
2. **Bounded disposable cache.** A configurable policy bounds `actcache/`
   by size and age. Eviction protects entries in use by any active engine,
   including engines of another Bosn daemon using the named volume. A pass
   removes exact selected entries and reports skipped protected bytes. It
   never clears the entire shared volume as a quota mechanism. Tools, runner
   tar, action checkouts and tool installs need explicit retention decisions
   and accounting so they cannot become an invisible remainder.
3. **Owned accounting.** Report measured bytes and counts for each shared
   cache component and repository namespace, active nested-engine storage,
   detached owned volumes, and host-side Bosn images and build cache where
   ownership can be established. Show largest contributors, retention reason,
   exact safe action, and whether measurement is partial. Never add shared
   layers and volume sizes into a false physical total. Continue reporting
   unowned artifacts through the existing `scan` path.
4. **Lifecycle and expiry.** Completed, failed, cancelled and interrupted runs
   eventually remove their exact nested engine and anonymous storage. Cleanup
   failure remains visible and retryable. Old Bosn-owned images and
   containers are expired only after exact ownership, reachability and use
   checks. Build cache needs a separate policy: Docker's build-cache records
   lack safe per-record deletion in the current census, so any prune must be
   scoped and independently verified before it is automated. Retained
   stack/machine and pinned volumes keep the explicit-release contract.
5. **Pressure response.** A threshold warning identifies the owned class
   causing pressure and distinguishes reclaimable from intentionally retained
   bytes. Unknown sizes remain unknown; an incomplete read cannot authorize
   deletion. The warning points to a supported Bosn command, not a broad
   Docker prune.

## Implementation sequence and verification

| Step | Work | Evidence required | State |
|---|---|---|---|
| 1 | Inventory current caches, accounting and cleanup; record a host sample | Code paths, current tests, read-only audit | Complete (survey above) |
| 2 | Expose a typed breakdown for the shared cache and owned engine volumes | Focused RED to GREEN tests with accurate partial/unknown behavior | Open |
| 3 | Add age/size policy for disposable act cache data and active-use coordination | Concurrent live runs retain hits; over-limit idle data shrinks; no cross-repo reads | Open |
| 4 | Account for and expire eligible old CI engines, host images and build cache | Fault/restart live Docker tests, exact ownership checks, repeated-run footprint trend | Open |
| 5 | Wire pressure diagnostics and verify sustained warm workloads | Repeated cold/warm benchmark plus disk growth under the configured ceiling | Open |

### Decisions to preserve

- Keep one named shared volume for cache data that must survive fresh engines;
  the host does not have a portable bind path into Docker Desktop's VM.
- Keep a private nested engine per run until a measured alternative preserves
  the socket/isolation and cleanup contract. Reusing a whole engine without
  a robust ownership boundary would let jobs inherit arbitrary state.
- Treat Docker's `reclaimable` estimate and `du` as different measurements.
  Report their provenance and avoid a misleading combined physical total.
- The existing `runners prune-cache` controls run records. Its name must not
  be presented as a shared cache eviction command.

### Still unverified

- Whether the second-run live restore test passes on the current host and
  whether it covers simultaneous runs. It is opt-in and needs Docker/network.
- Whether failed cleanup ever leaves anonymous nested-engine storage after
  daemon restart on the current Docker version. The active sample in #456
  does not answer this.
- Which detached stack/machine volumes remain useful and which registry owns
  them; do not infer from names or attachment alone.
