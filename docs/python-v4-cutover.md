# Offline Python-v4 state cutover

`bosn registry import-v4` is the one explicit bridge from a Python-v4
registry to the native v5 registry. It is an **offline copy**, not an engine
adoption or an in-place migration. The legacy database is never renamed,
modified, or deleted by Bosn.

```sh
bosn registry import-v4 \
  --legacy-state-dir /absolute/path/to/python-v4-state \
  --state-dir /absolute/path/to/new-native-state \
  --yes --json
```

The legacy directory must contain these bridge inputs:

```text
registry.sqlite3
rust-cutover-v1.json
```

The marker is a private, bounded JSON file written by the bridge-capable
Python-v4 cutover release. Its protocol is `1` and it names the exact
`registry_id` stored in the source database, for example:

```json
{"protocol":1,"registry_id":"11111111-2222-4333-8444-555555555555"}
```

Before invoking the native command, stop the old daemon and every old task,
then let the bridge-capable release relinquish the `registry.migration.lock`
that it shares with the importer. (The lock file itself may be created by the
importer.) Do not
manually manufacture a marker for an uncooperative older release. The native
importer takes the exclusive half of that same lock, refuses live or
uninspectable lease/session PIDs, and refuses a missing, malformed, or
registry-mismatched marker. This proves quiescence only for the cooperative
bridge protocol; it does not discover or stop arbitrary processes.

The source and destination must be distinct owner-private directories. A
destination `registry.sqlite3` is never overwritten, so a repeated command
is refused rather than producing a second mixed registry. Interrupted,
corrupt, unsupported, or busy imports leave no published destination database.
The importer first takes a consistent SQLite backup through `kernal-api`,
validates a closed v4 schema and all typed relations, writes v5 in private
staging, runs an integrity check, then publishes the destination without
overwriting it.

Every v4 table is copied with its original IDs and relationship keys:
`meta`, `resources`, `resource_uses`, `leases`, `execution_sessions`,
`volume_creation_intents`, `generations`, and `events`. In particular, pinned
volume resource IDs, engine names, retention, resource uses, and volume intent
labels remain durable facts. No Docker resource is looked up by name, adopted,
started, stopped, changed, or removed during this operation.

The imported registry has `migration.reconciliation_required=true`. Normal
native daemon startup and writer opening refuse this gate. A future explicit
engine-reconciliation workflow must compare exact durable IDs/provenance before
any lifecycle action. Do not clear that meta key manually and do not interpret
a successful import as proof that Docker's current state is safe to mutate.

The operation is deliberately unavailable through the daemon, Python client,
and MCP server: all of those are daemon control surfaces, whereas import must
run while the destination daemon is offline. Python installations receive the
same package-owned `bosn` executable, so this command is available without a
second Python lifecycle implementation.
