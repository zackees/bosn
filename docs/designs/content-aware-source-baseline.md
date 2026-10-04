# Verified source and output baseline handoff (CACHE-028)

Implementation design coordinated with ci.yml#281 and act2's sourcecheckout API.
This is not an activated cache path or an approved-writer implementation.

## Identity now available

A fresh frozen snapshot supplies the original commit, optional effective dirty
commit, SHA256 tree digest, and Git tree object of the effective commit. Admission
checks a supplied Git tree against that explicit commit in the staging repository.
The optional wire field preserves schema-1 requests and records; absent identity
means cold fallback. Older daemons with strict unknown-field rejection require a
matching client upgrade; no client silently downgrades an authenticated handoff.
Source identity grants no cache-writer authority.

## Receiver and durable baseline

The daemon-owned receiver must obtain an independent approved-writer grant from
pinned policy and verified run evidence. It binds repository/cache namespace,
immutable writer workflow and policy commits, run/attempt/job, original/effective
source commit, Git tree, tree digest, the compatible-output identity, and the
SHA256 of the complete captured source/output payload and metadata. Source or
workflow-controlled envelopes, HEAD and payload self-seals never approve writers.

Capture source and outputs from one quiescent owned filesystem baseline. Restore
that approved complete baseline into a new exclusive writable receiver directory,
then verify every inventoried source byte/type/executable bit/link against the
independent frozen Git object inventory. Cache .git is never authoritative.
The receiver installs independently verified authoritative Git metadata through a
separate staged replacement. Existing act2 verification currently supports bounded
loose Git objects; packed/alternate/missing object stores remain cold until a
bounded authenticated object transport is implemented.

Initial baseline extraction restores the captured donor metadata, including real
source mtimes, together with compatible Cargo/Dylint outputs. This preserves the
relationship Cargo fingerprints recorded. It is distinct from replaying timestamps
onto an already updated checkout. The subsequent checksum merge performs no writes
for identical content/type/executable bits/links; changed files receive fresh
writes and invalidate affected outputs. It must never invent old timestamps or
restore a donor timestamp onto changed source. Future donor clocks or inconsistent
fingerprint metadata fail cold.

## Bounded payload and compatible outputs

Use one versioned typed manifest plus bounded source/output streams, not arbitrary
recursive archive extraction. Source defaults match act2: at most 10,000 entries,
64 MiB per file and 256 MiB total; metadata and paths have explicit pre-allocation
bounds. Output families have separate declared byte/entry quotas, all charged to
the existing cache-family budget. Validate the entire manifest and transport digest
before publishing the owned baseline. Reject duplicate/case-colliding paths,
traversal, special files, unlisted hardlinks, escaping symlink chains and overlap
with .git or protected outputs. A failed validation or unsupported profile discards
the candidate and uses ordinary checkout; it does not widen limits or disable CI.

Compatible-output identity must bind actual compiler/toolchain and standard-library
artifact digests, target/host ABI, Cargo lock/dependency graph, features/profile,
resolved build command, relevant flags/environment, path/relocation contract, and
Dylint driver/lint-tree identity. Merely matching a source commit or family name is
insufficient. Source and outputs are admitted as one verified receipt, not combined
from unrelated writers or runs.

## Production integration and promotion acceptance

An initial useful receiver can restore a verified persistent owned baseline; the
end goal also requires actual cache archive delivery into a fresh engine. Wire the
receiver receipt to act2 HostEnvironment's preparation API, then integrate the
real own-repository CopyDir path and Docker source transport. Keep ordinary checkout
for missing, invalid or unsupported receipts. No public activation occurs before
measuring that real path.

RED/GREEN must cover actual CopyDir overwriting identical donor mtimes before the
merge, then preserving nanosecond mtime and inode after it; changed bytes/modes/link
types and tracked deletion; protected outputs and authoritative .git; dirty
synthetic commits; poisoned writer/payload/output receipts; quota failures; future
clocks and fingerprint inconsistency; and fresh-engine source/output restoration.
A serialized source API pass is not a workflow reuse measurement.

Physical PR-to-main promotion must transfer the approved source/output payload
under a default-branch cache identity after immutable approval, preserving the
original writer evidence and source/output binding. Renaming a key or inferring
trust from merged source metadata is insufficient. Required local gates and the
same primary reviewer precede shipping; no new workflows are required by this
slice.
