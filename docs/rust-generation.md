# Rust generation identity

`bosn-generation` collects a stack's selected Docker context and computes a
generation only after the caller supplies immutable image resolver receipts.
The crate is pinned to `kernal-api` commit
`fc634e507024d63ccaaf75fa564818b9dcfbff36` for this migration.

```rust
let stack = manifest.stack("web")?;
let generation = bosn_generation::stack_generation(
    &manifest, stack, materialization_root,
    &bosn_generation::collector::CollectorLimits::default(), &receipts,
)?;
```

The materialization root is resolved independently of workspace mounts. Docker
selection runs before ordinary file bytes are read; file, entry, path, depth,
and total-byte limits are explicit. Resolver receipts must be complete, unique,
and `sha256:` immutable identities. An image-only stack does not require a
Dockerfile or tree traversal.

This is not a sandbox or atomic filesystem snapshot. Ancestors must be trusted;
selected final-file races are rejected where observable, while directory-tree
changes and coarse timestamps can still evade detection. Windows has weaker
directory identity evidence than Unix. Python remains the production path and
jobs/lifecycle work is intentionally unfinished.
